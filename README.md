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
./scripts/local-dev.sh

# Start the API server (auto-runs migrations on first start)
cargo run --bin spectraplex-api
# → http://127.0.0.1:3000/health

# Run the smoke test (in another terminal)
./scripts/smoke-test.sh

# Or skip provider-dependent ingestion if you don't have live RPC access:
# ./scripts/smoke-test.sh --skip-ingest
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

## Supported Path vs Legacy Compatibility

Spectraplex has two operational paths:

**Supported MVP path (recommended):**
1. Register a target via API (`POST /v1/targets`)
2. Trigger ingestion via API (`POST /v1/ingest`)
3. Workers write V2 Bronze (`raw_transactions`) and auto-materialize Silver/Gold
4. Query or export from durable V2 tables

This path is fully tenant-scoped, survives restarts, and produces completeness/provenance metadata for every export.

**Legacy compatibility path (CLI `normalize`):**
- `spectraplex-cli normalize` parses V1 transactions and writes `ledger_entries` directly
- API workers optionally write V1 tables (`transactions`, `indexer_checkpoints`) for backward compatibility
- Controlled by `enable_v1_compat_writes` config (default: `true`)

Operators who no longer need V1 tables can set `enable_v1_compat_writes = false` in `spectraplex.toml`. The CLI `normalize` command remains available for offline/file-based workflows but is not the recommended pipeline.

## API

All `/v1/*` routes require `Authorization: Bearer <API_KEY>`.

Tenant-scoped API keys (created via `POST /v1/api-keys`) are required for dataset queries and exports. The legacy config key (`SPECTRAPLEX_API_KEY`) can create tenant keys but does not itself support tenant-scoped endpoints.

### Ingestion and Jobs

```bash
# Trigger ingestion (wallet + network; API auto-creates the target)
curl -X POST http://127.0.0.1:3000/v1/ingest \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"wallet": "<ADDRESS>", "network": "solana-mainnet"}'

# Check job status
curl -H "Authorization: Bearer $API_KEY" \
  http://127.0.0.1:3000/v1/jobs/<JOB_ID>
```

Ingestion jobs are durable — they persist across restarts and are claimed by background workers with heartbeat-based lease management.

### Real-Time Streaming

```bash
# Start a Solana gRPC stream (requires SOLANA_GRPC_URL)
curl -X POST http://127.0.0.1:3000/v1/stream/start \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"chain": "solana"}'

# Start a Hyperliquid WebSocket stream for a wallet
curl -X POST http://127.0.0.1:3000/v1/stream/start \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"chain": "hyperliquid", "wallet": "0x..."}'

# List active streams
curl -H "Authorization: Bearer $API_KEY" \
  http://127.0.0.1:3000/v1/streams

# Stop a stream
curl -X POST http://127.0.0.1:3000/v1/stream/<STREAM_ID>/stop \
  -H "Authorization: Bearer $API_KEY"
```

Stream subscriptions are durable and create `ingestion_runs` per flush batch with full Bronze lineage. Hyperliquid streams subscribe to user fills, funding, and ledger updates via WebSocket with automatic reconnection.

### Targets and Networks

```bash
# Register an indexing target
curl -X POST http://127.0.0.1:3000/v1/targets \
  -H "Authorization: Bearer $SPECT...KEY" \
  -H "Content-Type: application/json" \
  -d '{"kind": "wallet", "network": "solana-mainnet", "address": "<ADDRESS>", "mode": "both"}'

# List networks
curl -H "Authorization: Bearer $SPECT...KEY" http://127.0.0.1:3000/v1/networks
```

### API Keys

Spectraplex supports two authentication modes:
- **Legacy config key** — set via `SPECTRAPLEX_API_KEY` or `api_key` in config. Full admin access, no tenant isolation.
- **Tenant-scoped keys** — created via the API, stored in Postgres, isolated by owner.

```bash
# Create a tenant-scoped API key (use legacy key for auth)
curl -X POST http://127.0.0.1:3000/v1/api-keys \
  -H "Authorization: Bearer $SPECT...CONFIG_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name": "production"}'
# → returns {"id":"...","key":"spx_...","owner_id":"..."}

# List your active keys
curl -H "Authorization: Bearer $SPX...TENANT_KEY" \
  http://127.0.0.1:3000/v1/api-keys

# Revoke a key
curl -X DELETE http://127.0.0.1:3000/v1/api-keys/<KEY_ID> \
  -H "Authorization: Bearer $SPX...TENANT_KEY"
```

### Query Datasets

Tenant-scoped dataset queries require `target_id`:

```bash
# Query a dataset (tenant-scoped)
curl -H "Authorization: Bearer $API_KEY" \
  "http://127.0.0.1:3000/v1/datasets/token_transfers/records?target_id=<TARGET_ID>&network=solana-mainnet&limit=50"

# Check dataset completeness
curl -H "Authorization: Bearer $API_KEY" \
  http://127.0.0.1:3000/v1/datasets/token_transfers/completeness

# Check dataset status (aggregated across targets)
curl -H "Authorization: Bearer $API_KEY" \
  http://127.0.0.1:3000/v1/datasets/token_transfers/status
```

Dataset completeness is tracked per target and network. After each materialization run, the system upserts a completeness record with coverage bounds (timestamp or block range), record count, and status (`complete`, `partial`, `backfilling`, or `gap`).

### Export

Tenant-scoped exports require `target_id`:

```bash
# Create an export job (CSV or JSONL)
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"dataset": "wallet_ledger", "format": "csv", "target_id": "<TARGET_ID>", "network": "solana-mainnet"}'

# Download when ready
curl -H "Authorization: Bearer $API_KEY" \
  http://127.0.0.1:3000/v1/export/jobs/<JOB_ID>/download
```

Export jobs write two files to the configured export directory:
- `{job_id}.csv` or `{job_id}.jsonl` — the data artifact
- `{job_id}.provenance.json` — sidecar with dataset version, completeness status, coverage bounds, and record count

The provenance file is useful for downstream pipelines that need to know whether the export is complete or partial before processing.

Export jobs are durable with heartbeat-based worker execution. Supported sinks: `local_file` and `webhook`.

### Callback HMAC Verification

When `callback_hmac_secret` is configured, callback payloads include an `X-Spectraplex-Signature` header:

```bash
curl -X POST http://127.0.0.1:3000/v1/ingest \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"wallet": "<ADDRESS>", "network": "solana-mainnet", "callback_url": "https://your-app.example.com/webhook"}'
```

The webhook receiver can verify the signature:

```python
import hmac
import hashlib

def verify_signature(payload: bytes, signature_header: str, secret: str) -> bool:
    expected = hmac.new(secret.encode(), payload, hashlib.sha256).hexdigest()
    return hmac.compare_digest(f"sha256={expected}", signature_header)
```

Signature format: `X-Spectraplex-Signature: sha256=<hex>`.

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

**API keys:**
`POST /v1/api-keys` | `GET /v1/api-keys` | `DELETE /v1/api-keys/:key_id`

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
SPECTRAPLEX_API_KEY=***
SOLANA_RPC_URL=https://api.mainnet-beta.solana.com
EVM_RPC_URL=https://eth.llamarpc.com
# Optional
SOLANA_GRPC_URL=https://your-yellowstone-endpoint
SOLANA_GRPC_TOKEN=***
# Optional: disable V1 compatibility writes
# SPECTRAPLEX_ENABLE_V1_COMPAT_WRITES=false
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
