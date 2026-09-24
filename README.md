# Spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

**Your self-hosted blockchain data workbench.** Explore wallet activity across Solana, Ethereum-family networks and Hyperliquid. Import trusted data, inspect transactions, and download CSV or JSONL—with provenance. Browser UI, REST API and CLI; no telemetry or frontend build required.

## Start here

Requires Docker with Compose v2 (Windows: use WSL). Allow roughly 4 GB RAM and 10 GB free disk for the first build, which takes several minutes.

```bash
git clone https://github.com/user1303836/spectraplex.git
cd spectraplex
./scripts/setup.sh
docker compose up --build -d --wait
```

Open **http://localhost:3000**. Connect using the value of `SPECTRAPLEX_API_KEY` in `.env`.

1. Click **Solana**, **Ethereum** or **Hyperliquid** under sample data. No RPC account or funds needed.
2. Choose a dataset, filter dates, or open **View JSON**. Normalized results appear when the background job finishes.
3. Click **Export CSV** or **Export JSONL**. Download the file and its provenance from **Activity**.

Samples are synthetic and isolated from real networks. Reloading them is safe. Keys stay in browser memory; reloading disconnects you.

**Real data:** set your RPC URLs in `spectraplex.toml`, run `docker compose restart api`, then **Add a target → Fetch activity**. Public RPCs may throttle requests. Solana's **Fetch older** walks backwards through history. EVM targets accept an optional starting block and scan forwards in bounded batches; the default is recent blocks. `ingest_limit` controls each batch (EVM: blocks; Solana: transactions).

**Sharing:** create tenant keys in **API keys**, not copies of your admin key. Admin-created keys start isolated workspaces; tenant-created keys share that tenant's workspace. Administrators can access all data.

**Operations:** `/ready` checks the database. `docker compose logs --tail 100 api` shows diagnostics. PostgreSQL and exports persist in named volumes; back up both. `docker compose down -v` deletes them. If ports are occupied, set `SPECTRAPLEX_PORT` or `POSTGRES_PORT` in `.env`. Keep the local credentials private; use TLS and hardened database credentials before exposing the service remotely.

## What you can work with

| Input | Useful datasets |
|---|---|
| Solana wallets, including v0 transactions | Raw transactions, token transfers, native balance deltas, wallet ledger, indexed balance history |
| EVM wallets | Raw transactions, ERC20 transfers, top-level native value and execution gas, wallet ledger, indexed balance history |
| Hyperliquid wallets | Fills, funding, deposits/withdrawals, wallet ledger; beta PnL and trade history |
| Hyperliquid markets; EVM contracts/topic filters via API | Raw transaction query/export |
| Trusted JSON/JSONL imports (admin only) | The same durable ingestion → normalization → export pipeline |

The dataset selector and `GET /v1/datasets` list available datasets. Configure additional networks/providers using `spectraplex.toml.example`.

**Know the limits:**
- This is bounded indexing, not a complete-chain index or verified account balance service. EVM scans stop 12 blocks behind the head; that is not a reorg/finality guarantee. Hyperliquid providers may retain only limited history.
- A “complete” dataset status means its indexed batch was processed, not that all historical activity was found. Inspect coverage and export provenance.
- EVM wallet accounting excludes internal transfers, NFTs, and fees beyond execution gas. Unknown ERC20 decimals are `-1`: amounts remain raw units and are excluded from financial totals.
- Balances are cumulative changes in indexed events. PnL/trade grouping is beta; protocol/TVL analytics and live streaming are experimental. Exports are not tax returns.
- Imports are trusted, not independently chain-verified. Existing Bronze payloads are immutable. Older arrivals can rebuild known balance deltas; legacy snapshots without deltas fail closed rather than invent history.

## API: sample → query → download

Requires `curl` and `jq`. Run from the repository after starting the service:

```bash
source .env
BASE="http://127.0.0.1:${SPECTRAPLEX_PORT:-3000}"
api() { curl -fsS -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H 'Content-Type: application/json' "$@"; }

# Raw records are available immediately; normalization runs in the background.
SAMPLE=$(api -X POST "$BASE/v1/demo/ethereum" -d '{}')
TARGET=$(printf '%s' "$SAMPLE" | jq -r .target_id)
api "$BASE/v1/datasets/raw_transactions/records?target_id=$TARGET&limit=50" | jq

EXPORT=$(api -X POST "$BASE/v1/export/dataset" \
  -d "{\"dataset\":\"raw_transactions\",\"target_id\":\"$TARGET\",\"format\":\"jsonl\"}" | jq -r .id)
for attempt in $(seq 1 60); do
  STATE=$(api "$BASE/v1/export/jobs/$EXPORT" | jq -r .state)
  [[ "$STATE" == completed || "$STATE" == failed ]] && break
  sleep 1
done
api "$BASE/v1/export/jobs/$EXPORT" | jq
if [[ "$STATE" == completed ]]; then
  api "$BASE/v1/export/jobs/$EXPORT/download" -o history.jsonl
  api "$BASE/v1/export/jobs/$EXPORT/download?provenance=true" -o history.provenance.json
fi
```

Use `wallet_ledger`, `token_transfers` or another dataset name for normalized results after materialization completes. Record queries support `target_id`, `network`, `time_start`/`time_end` (Unix seconds), `limit` and `offset`.

| Action | Route |
|---|---|
| Create/list targets | `POST` / `GET /v1/targets` |
| Fetch a target | `POST /v1/targets/{id}/ingest` with `{"mode":"incremental"}` or `{"mode":"backfill"}` |
| Inspect jobs | `GET /v1/jobs`, `GET /v1/jobs/{id}` |
| Create a tenant key | `POST /v1/api-keys` with `{"name":"Research"}` |
| Revoke a key | `DELETE /v1/api-keys/{id}` |

### Import your own records

Create a real-network target, then open **Import raw transactions**. Accepts JSON arrays, `{"records":[…]}`, or JSONL; at most 500 records / 1 MiB per upload. Each record needs `tx_hash`, `timestamp` (Unix seconds), and a chain-specific `raw_metadata` RPC payload. Optional: `block_number`. Download sample payloads from the sidebar to see the shapes; don't present synthetic records as real activity.

For an existing real target and a trusted `records.jsonl` file, the equivalent API request is:

```bash
read -r -p 'Real target ID: ' REAL_TARGET
jq -s '{records: .}' records.jsonl | \
  api -X POST "$BASE/v1/targets/$REAL_TARGET/import" --data-binary @-
```

## CLI and development

```bash
docker compose exec api spectraplex-cli --help
docker compose exec api spectraplex-cli list-networks
```

Prefer the workbench/API for the canonical pipeline. CLI `normalize` is a legacy compatibility path, not the workbench materializer.

To run natively on macOS/Linux: install **Rust 1.94+** and **PostgreSQL 16+** (or start just `docker compose up -d postgres`), run `./scripts/setup.sh`, set `DATABASE_URL` in `.env`, then:

```bash
cargo run --locked --bin spectraplex-api
```

Config loads from `spectraplex.toml` (or `SPECTRAPLEX_CONFIG`), then environment overrides. The API applies migrations automatically. Use a **local test database account with CREATEDB permission** for integration tests; PostgreSQL's `psql` and Python 3 must be on PATH:

```bash
source .env
export TEST_DATABASE_URL="$DATABASE_URL"
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
npm ci --prefix web-tests
(cd web-tests && npx playwright install chromium)
./scripts/smoke-test.sh --browser
```

Node 22+ is only needed for browser tests. The smoke suite uses isolated databases and local provider fixtures, covering three-chain ingestion, financial fixtures, replay, tenant isolation, imports, exports, restart recovery, mobile layout and automated WCAG AA checks. CI also boots containers and checks volume persistence. Reviewed upstream security exceptions live in `.cargo/audit.toml`.

MIT licensed. Rust workspace: `core/` models, `adapters/` ingestion/storage, `api/` workbench/workers, `cli/` administration, `migrations/` schema.
