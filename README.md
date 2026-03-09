# Spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

Spectraplex is a multi-chain blockchain indexing and normalization system written in Rust. It ingests raw data from supported chains, normalizes that data into reusable downstream datasets, and serves it through both a CLI and REST API. Built around a **Bronze/Silver data layer architecture**, it is intended to become a general-purpose indexing layer for ETL and analytics workflows, while still keeping wallet-centric indexing first-class for tax, portfolio, and forensics use cases.

## Project Direction

Today, the implementation is strongest around wallet-centric ingestion and ledger normalization. That remains an important capability, but it is not the intended end state of the project.

The long-term direction is a **target-centric indexing platform** that can support:

- wallet indexing
- contract and program indexing
- protocol and pool indexing
- Hyperliquid market and user analytics
- downstream ETL, warehousing, and dashboard pipelines

For the current strategy and execution plan, see [`SPECTRAPLEX_STRATEGY_AND_EXECUTION_PLAN.md`](SPECTRAPLEX_STRATEGY_AND_EXECUTION_PLAN.md).

For the condensed external landscape and source list, see [`INDEXER_RESEARCH.md`](INDEXER_RESEARCH.md).

## Supported Chains

| Chain | Ingestion | Parsing | Real-time Streaming | Status |
|-------|-----------|---------|---------------------|--------|
| Solana | RPC + gRPC | SOL + SPL tokens (symbol lookup) | Yellowstone gRPC | Active |
| Hyperliquid | REST + WebSocket | Fills, deposits, withdrawals | WebSocket | Active |
| Ethereum (EVM) | alloy (eth_getLogs) | ERC-20 transfers (uint256 precision) | Planned | Active |

Current CLI and API workflows are mostly wallet-oriented. Broader target types are part of the planned architecture.

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

1. **Ingest (Bronze)** -- Fetch raw chain data from RPC, gRPC, REST, or WebSocket endpoints and store it durably.
2. **Normalize (Silver)** -- Parse raw data into reusable normalized datasets. Today the main implemented Silver dataset is `ledger_entries`.
3. **Query / Export** -- Retrieve indexed data through the API or CLI. Today the query surface is mostly wallet-oriented; broader ETL-oriented delivery is planned.

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
# Health check (no auth required)
curl http://127.0.0.1:3000/health

# Get raw transactions for a wallet (paginated, with optional date range)
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/transactions/<WALLET_ADDRESS>?limit=50&offset=0"

# Get normalized ledger entries (paginated)
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/ledger/<WALLET_ADDRESS>?limit=50&offset=0"

# Get current balances
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/balances/<WALLET_ADDRESS>"

# Export ledger as CSV
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/export/<WALLET_ADDRESS>?format=csv"

# Get wallet stats
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/stats/<WALLET_ADDRESS>"
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

All `/v1/*` endpoints require authentication via `Authorization: Bearer <API_KEY>` header. The API key is configured server-side via `SPECTRAPLEX_API_KEY`. If no API key is configured, all requests are rejected (fail-closed).

### Which API Should I Use?

Spectraplex exposes two complementary API surfaces:

- **Wallet-scoped endpoints** (`/v1/transactions/:wallet`, `/v1/ledger/:wallet`, etc.) provide familiar, wallet-centric access to raw and normalized data. These endpoints are **stable** and will continue to be maintained for backward compatibility. They are well suited for tax, portfolio, and forensics use cases where the primary query axis is a single wallet address.

- **Dataset endpoints** (`/v1/datasets/...`, `/v1/export/dataset`, etc.) are the **preferred forward path** for new consumers. They offer flexible, target-agnostic querying across Silver datasets, support richer filtering (by target, network, and time range), and integrate cleanly with ETL and data warehouse pipelines. New integrations should prefer dataset endpoints unless the use case is strictly single-wallet.

Both API surfaces share the same authentication and authorization model, and both remain fully supported.

### Ingestion and Job Control

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check, returns `"OK"` (no auth required) |
| `POST` | `/v1/ingest` | Trigger async ingestion for a wallet (returns job ID) |
| `POST` | `/v1/ingest/batch` | Trigger batch ingestion for multiple wallets (max 50) |
| `POST` | `/v1/normalize` | Trigger async normalization for a wallet (returns job ID) |
| `GET` | `/v1/jobs/:job_id` | Poll job status (pending/running/completed/failed) |
| `POST` | `/v1/stream/start` | Start real-time Solana gRPC streaming (returns stream ID) |
| `POST` | `/v1/stream/:stream_id/stop` | Stop an active stream |
| `GET` | `/v1/streams` | List all active streams with stats |

The ingest and normalize endpoints accept an optional `callback_url` field. When provided, the server will POST a JSON payload to that URL when the job completes or fails. Only HTTP(S) URLs targeting public addresses are accepted.

### Wallet-Scoped Query Endpoints (Stable)

These endpoints query data by wallet address. They are stable and maintained for backward compatibility.

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/v1/transactions/:wallet` | Get raw transactions by wallet (paginated) |
| `GET` | `/v1/transactions/:wallet/:tx_hash` | Get a single transaction by hash |
| `GET` | `/v1/ledger/:wallet` | Get normalized ledger entries by wallet (paginated) |
| `GET` | `/v1/export/:wallet` | Export ledger entries as CSV or JSON |
| `GET` | `/v1/balances/:wallet` | Get current asset balances (aggregated from ledger) |
| `GET` | `/v1/stats/:wallet` | Get wallet statistics (tx count, chains, date range) |

All wallet endpoints validate the address format and return structured JSON errors.

Query endpoints support `?limit=N&offset=N` parameters (default limit: 50, max: 1000).

The transactions, ledger, and export endpoints support date range filtering via `?from=<unix_ts>&to=<unix_ts>` query parameters (both optional).

### Dataset Query and Export Endpoints (Preferred)

These endpoints are the preferred forward path for new consumers. They provide flexible, target-agnostic querying across Silver datasets and integrate with ETL workflows.

| Method | Endpoint | Description |
|--------|----------|-------------|
| `POST` | `/v1/targets` | Register an index target (wallet, contract, program, etc.) |
| `GET` | `/v1/targets` | List all registered index targets |
| `GET` | `/v1/targets/:target_id` | Get a specific index target by ID |
| `GET` | `/v1/networks` | List all known networks |
| `GET` | `/v1/networks/:network_id` | Get a specific network by ID |
| `GET` | `/v1/datasets` | List all datasets with latest version info |
| `GET` | `/v1/datasets/:name/versions` | List version history for a dataset |
| `GET` | `/v1/datasets/:name/records` | Query dataset records with filters (paginated) |
| `GET` | `/v1/datasets/:name/completeness` | Get completeness status for a dataset |
| `GET` | `/v1/datasets/:name/status` | Materialization status: active version, all versions, completeness |
| `POST` | `/v1/export/dataset` | Create an async export job for a dataset (JSONL or CSV) |
| `GET` | `/v1/export/jobs/:job_id` | Poll export job status |
| `GET` | `/v1/export/jobs/:job_id/download` | Download completed export data |
| `GET` | `/v1/export/tax` | Export wallet_ledger as tax-software-friendly CSV |
| `GET` | `/v1/forensics/activity` | Wallet interaction analysis (top counterparties, cross-chain summary) |

The dataset records endpoint supports filtering via `?target_id=<UUID>&network=<id>&time_start=<unix_ts>&time_end=<unix_ts>&limit=N&offset=N` query parameters (all optional). Queryable datasets: `token_transfers`, `native_balance_deltas`, `decoded_events`, `hl_fills`, `hl_funding`, `positions`, `wallet_ledger`, `balance_history`.

### POST /v1/ingest

```bash
curl -X POST http://127.0.0.1:3000/v1/ingest \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"chain": "solana", "wallet": "<WALLET>"}'

# Response: {"id": "<JOB_UUID>", "state": "pending", "message": "Job queued"}
```

Optional: add `"callback_url": "https://example.com/webhook"` to receive a POST notification when the job finishes.

### POST /v1/ingest/batch

```bash
curl -X POST http://127.0.0.1:3000/v1/ingest/batch \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"wallets": [{"chain": "solana", "wallet": "<WALLET_1>"}, {"chain": "ethereum", "wallet": "<WALLET_2>"}]}'

# Response: [{"id": "<JOB_UUID>", "state": "pending", "message": "Job queued"}, ...]
```

Batch size is capped at 50 wallets per request.

### POST /v1/normalize

```bash
curl -X POST http://127.0.0.1:3000/v1/normalize \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"wallet": "<WALLET>"}'

# Response: {"id": "<JOB_UUID>", "state": "pending", "message": "Job queued"}
```

### GET /v1/jobs/:job_id

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/jobs/<JOB_UUID>

# Response: {"id": "<JOB_UUID>", "state": "completed", "message": "Ingested 42 transactions"}
```

Jobs are kept in memory for 1 hour after completion, then automatically pruned.

### GET /v1/transactions/:wallet/:tx_hash

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/transactions/<WALLET>/0xdeadbeef

# Response: {"id": "...", "wallet_address": "...", "tx_hash": "0xdeadbeef", ...}
# Returns 404 if not found
```

### GET /v1/export/:wallet

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/export/<WALLET>?format=csv"

# Response: CSV file with headers: id,transaction_id,wallet_address,asset_symbol,amount,entry_type,fiat_value
```

Supports `?format=csv` (default) or `?format=json`. Maximum 10,000 entries per export. Supports `?from=<unix_ts>&to=<unix_ts>` for date range filtering.

### GET /v1/balances/:wallet

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/balances/<WALLET>

# Response: [{"asset_symbol": "SOL", "balance": "12.5"}, {"asset_symbol": "USDC", "balance": "1000"}]
```

Returns aggregated balances from all ledger entries. Supports `?at=<unix_ts>` for point-in-time balance snapshots.

### GET /v1/stats/:wallet

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/stats/<WALLET>

# Response: {"total_transactions": 42, "earliest_timestamp": 1700000000, "latest_timestamp": 1700100000, "total_chains": 2, "unique_assets": 5, "transactions_per_chain": [{"chain": "solana", "count": 30}, ...]}
```

### POST /v1/stream/start

```bash
curl -X POST http://127.0.0.1:3000/v1/stream/start \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"chain": "solana"}'

# Response: {"id": "<STREAM_UUID>", "uptime_secs": 0, "transactions_ingested": 0, "last_slot": 0}
```

Starts a real-time Solana gRPC streaming session. Requires `SOLANA_GRPC_URL` to be configured. Transactions are batched (100 per batch or every 5 seconds) and saved to the database. Maximum 5 concurrent streams.

### POST /v1/stream/:stream_id/stop

```bash
curl -X POST -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/stream/<STREAM_UUID>/stop

# Response: {"id": "<STREAM_UUID>", "status": "stopping"}
```

### GET /v1/streams

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/streams

# Response: [{"id": "...", "uptime_secs": 120, "transactions_ingested": 5000, "last_slot": 300000}]
```

Lists all active streams with their current statistics.

### GET /v1/datasets

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/datasets

# Response: [{"name": "token_transfers", "latest_version": 1, "latest_version_status": "complete"}, ...]
```

Lists all Silver datasets with their latest version info.

### GET /v1/datasets/:name/versions

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/datasets/token_transfers/versions

# Response: [{"id": "...", "dataset_name": "token_transfers", "version": 1, "status": "complete", ...}]
```

Returns the version history for a specific dataset.

### GET /v1/datasets/:name/records

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/datasets/token_transfers/records?network=solana-mainnet&limit=50"

# Response: [{"tx_hash": "...", "mint": "So11...", "from_owner": "...", "to_owner": "...", "amount": "1000000000", ...}]
```

Query dataset records with optional filters. Supported query parameters: `target_id`, `network`, `time_start`, `time_end`, `limit` (default 100, max 1000), `offset`. Queryable datasets: `token_transfers`, `native_balance_deltas`, `decoded_events`, `hl_fills`, `hl_funding`, `positions`.

### GET /v1/datasets/:name/completeness

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/datasets/token_transfers/completeness

# Response: [{"target_id": "...", "dataset_name": "token_transfers", "network": "solana-mainnet", "status": "complete", ...}]
```

Returns completeness tracking records for a dataset.

### POST /v1/export/dataset

```bash
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"dataset": "token_transfers", "format": "jsonl"}'

# Response: {"id": "<JOB_UUID>", "state": "pending", "dataset": "token_transfers", "format": "jsonl", "record_count": null, "message": null}
```

Creates an async export job for a dataset. Supported formats: `jsonl` (default), `csv`. Optional filters: `target_id`, `network`, `time_start`, `time_end`. Exportable datasets: `token_transfers`, `native_balance_deltas`, `decoded_events`, `hl_fills`, `hl_funding`, `positions`, `wallet_ledger`, `balance_history`. Maximum 100,000 records per export.

#### Sink delivery

Export jobs support an optional `sink` field to deliver completed export data to an external destination. When a sink is configured, data is delivered to the sink **and** stored in-memory for download via the existing `/v1/export/jobs/:job_id/download` endpoint (backward compatible).

Supported sink types:

| Sink type | Description | Required fields |
|-----------|-------------|-----------------|
| `local_file` | Write export data to a local file path | `file_path` |
| `webhook` | POST export data to an HTTP(S) URL | `url`, optional `headers` |
| `database` | Deliver to an external database (not yet implemented) | `connection_string`, `table` |

Example with local file sink:

```bash
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "dataset": "token_transfers",
    "format": "jsonl",
    "sink": {
      "sink_type": "local_file",
      "file_path": "/tmp/token_transfers_export.jsonl"
    }
  }'
```

Example with webhook sink:

```bash
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{
    "dataset": "hl_fills",
    "format": "csv",
    "sink": {
      "sink_type": "webhook",
      "url": "https://example.com/receive-export",
      "headers": {"X-API-Key": "your-key"}
    }
  }'
```

When a sink is configured, the export job status response includes additional fields:

- `delivered_to`: destination description (file path, URL, etc.) — set on successful delivery
- `delivery_status`: one of `"pending"`, `"delivered"`, or `"failed"`

Webhook URLs are validated against the same rules as `callback_url` (HTTPS/HTTP only, no private/loopback addresses). Local file paths must not contain `..` path traversal. The `database` sink type is reserved but not yet implemented at runtime.

### GET /v1/datasets/:name/status

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/datasets/token_transfers/status
```

Returns materialization status for a dataset: the active version, all known versions, and completeness records across targets. Useful for downstream consumers to determine which Materializer version produced the current data and whether coverage is complete.

Response fields:

| Field | Description |
|-------|-------------|
| `name` | Dataset name |
| `active_version` | Active version details (null if no active version) |
| `versions` | All versions ordered by version number descending |
| `completeness` | Completeness records across all targets |

### GET /v1/export/jobs/:job_id

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/export/jobs/<JOB_UUID>

# Response: {"id": "...", "state": "completed", "dataset": "token_transfers", "format": "jsonl", "record_count": 1234, "message": "Exported 1234 records", "dataset_version_id": "...", "dataset_version": 2, "completeness_status": "complete", ...}
```

Returns the status of an export job. States: `pending`, `running`, `completed`, `failed`.

When the job completes, the response includes enriched metadata fields for provenance and observability:

| Field | Description |
|-------|-------------|
| `dataset_version_id` | UUID of the active dataset version used for the export |
| `dataset_version` | Version number of the active dataset version |
| `completeness_status` | Aggregated completeness: `complete`, `partial`, `backfilling`, or `gap` |
| `completeness_coverage` | JSON object with `coverage_start`, `coverage_end`, `block_start`, `block_end` |
| `started_at` | ISO 8601 wall-clock timestamp when the export started |
| `completed_at` | ISO 8601 wall-clock timestamp when the export finished |
| `last_ingestion_run_id` | UUID of the most recent ingestion run that contributed to the data |

All metadata fields use `skip_serializing_if` so they are omitted from the response when not available (backward compatible).

### GET /v1/export/jobs/:job_id/download

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  http://127.0.0.1:3000/v1/export/jobs/<JOB_UUID>/download \
  -o export.jsonl

# Returns the export data with appropriate Content-Type and Content-Disposition headers.
# Returns 409 Conflict if the job is still running, or 400 if it failed.
```

Downloads the completed export data. The response includes a `Content-Disposition` header with the filename.

### Gold Datasets: wallet_ledger and balance_history

Spectraplex includes two Gold-tier datasets materialized from Silver data:

- **`wallet_ledger`** — wallet-scoped financial records with counterparty tracking, network awareness, and nullable cost basis / proceeds fields. Derived from `token_transfers`, `native_balance_deltas`, `hl_fills`, and `hl_funding`. Queryable and exportable via the standard dataset API.

- **`balance_history`** — per-wallet, per-asset running balance snapshots derived from wallet_ledger entries. Enables point-in-time balance queries for forensics and portfolio tracking.

Both datasets are queryable via `/v1/datasets/{name}/records` and exportable via `/v1/export/dataset`.

### GET /v1/export/tax

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/export/tax?target_id=<UUID>" \
  -o tax-export.csv
```

Exports wallet_ledger records as a tax-software-friendly CSV with columns:

`Date, Type, Sent_Asset, Sent_Amount, Received_Asset, Received_Amount, Fee_Asset, Fee_Amount, Cost_Basis, Proceeds, Gain_Loss, Tx_Hash, Network`

Cost_Basis, Proceeds, and Gain_Loss are nullable — they are populated when future tax lot matching is available. Supports `?target_id`, `?network`, `?time_start`, `?time_end` filters.

### GET /v1/forensics/activity

```bash
curl -H "Authorization: Bearer <API_KEY>" \
  "http://127.0.0.1:3000/v1/forensics/activity?target_id=<UUID>"

# Response: {"wallet_address": "...", "top_counterparties": [...], "network_activity": [...], "type_breakdown": [...], "total_entries": 42}
```

Returns a forensics activity summary for wallet_ledger records: top counterparties by interaction count, cross-chain activity breakdown, and transaction type distribution. Supports `?target_id`, `?network`, `?time_start`, `?time_end` filters.

## Configuration

Spectraplex uses a layered configuration system powered by [figment](https://crates.io/crates/figment). Settings are loaded in order of priority:

1. Built-in defaults
2. `spectraplex.toml` (optional config file in project root)
3. `SPECTRAPLEX_*` environment variables (e.g., `SPECTRAPLEX_PORT=8080`)
4. Direct env vars: `DATABASE_URL`, `SOLANA_RPC_URL`, `EVM_RPC_URL`, `SOLANA_GRPC_URL`, `SOLANA_GRPC_TOKEN`

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | *(required)* | PostgreSQL connection string |
| `SPECTRAPLEX_HOST` | `127.0.0.1` | API server bind address |
| `SPECTRAPLEX_PORT` | `3000` | API server port |
| `SPECTRAPLEX_POOL_SIZE` | `10` | Database connection pool size |
| `SPECTRAPLEX_LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `SPECTRAPLEX_INGEST_LIMIT` | `50` | Default transaction fetch limit for API ingestion |
| `SPECTRAPLEX_API_KEY` | *(none)* | API key for authenticating requests. If unset, all requests are rejected. |
| `SPECTRAPLEX_ALLOWED_WALLETS` | *(none)* | Comma-separated list of wallet addresses to restrict access to. If unset, all wallets are allowed. |
| `SOLANA_RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana RPC endpoint |
| `EVM_RPC_URL` | `https://eth.llamarpc.com` | EVM JSON-RPC endpoint |
| `SOLANA_GRPC_URL` | *(none)* | Yellowstone gRPC endpoint for real-time streaming |
| `SOLANA_GRPC_TOKEN` | *(none)* | Auth token for the gRPC endpoint |

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
| API Framework | Axum 0.8 |
| CLI Framework | Clap 4 (derive) |
| Solana | solana-sdk 3.0, Yellowstone gRPC |
| Ethereum (EVM) | alloy 1.x, governor (rate limiting) |
| Serialization | serde / serde_json |
| Precision Math | BigDecimal |

## Roadmap

- [x] Build a working multi-chain Rust workspace with Bronze/Silver storage, CLI, API, and chain adapters
- [x] Support wallet-centric ingestion and normalization for Solana, Hyperliquid, and EVM-compatible chains
- [x] Add operational basics: auth, checkpoints, exports, streaming hooks, CI, Docker, and test coverage
- [ ] Refactor the Bronze model so raw chain data is canonical and not shaped around a single wallet or user
- [ ] Introduce a general index target model: wallet, contract, program, topic filter, market, pool, protocol
- [ ] Split chain family from network identity so EVM-compatible networks are modeled correctly
- [ ] Expand Silver beyond `ledger_entries` into reusable datasets like transfers, decoded events, fills, swaps, and balance snapshots
- [ ] Add ETL-first delivery modes such as dataset exports, sink jobs, and warehouse-friendly outputs
- [x] Keep wallet/tax/forensics materializations first-class, but as downstream products of the broader indexing core (P5-W1: wallet_ledger, balance_history, tax export, forensics activity)

The detailed roadmap lives in [`SPECTRAPLEX_STRATEGY_AND_EXECUTION_PLAN.md`](SPECTRAPLEX_STRATEGY_AND_EXECUTION_PLAN.md).

## License

See [LICENSE](LICENSE) for details.
