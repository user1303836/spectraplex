# P0-W1 V2 Architecture RFC Handoff

Status: **Frozen**
Date: 2026-03-08
Phase: P0-W1
Branch: `codex/p0-w1-v2-architecture-rfc`
Depends on: None (first work packet)
Downstream dependents: P0-W2 (Target Model), P0-W3 (Network Model), P0-W4 (Rollout Plan), all Phase 1+

This document is the tracked, reviewable artifact for the P0-W1 V2 Architecture
RFC. It captures the locked decisions from the full RFC (`design/V2_ARCHITECTURE_RFC.md`,
which lives in the gitignored `design/` directory) so that reviewers can validate
the architectural contract on GitHub without accessing local-only design docs.

No new scope is introduced here. Every item below is drawn directly from the
frozen `design/V2_ARCHITECTURE_RFC.md` (604 lines).

---

## 1. Locked Decisions (RFC Section 7)

These decisions are locked by this RFC. Later work packets should reference them,
not reopen them.

| # | Decision | Choice | Rationale |
|---|---|---|---|
| 1 | Bronze raw data is target-agnostic | No `user_id` or `wallet_address` on raw rows | Canonical chain facts must be reusable across targets |
| 2 | Target linkage via join table | `target_matches(target_id, raw_transaction_id)` | One raw tx can belong to many targets |
| 3 | Target kinds include `account` | `wallet, contract, program, account, topic_filter, market, pool, protocol` | Solana PDAs and data accounts are valid indexing targets distinct from wallets |
| 4 | Network replaces Chain | `chain_family` + `network` instead of flat `Chain` enum | Base, Arbitrum, HyperEVM are not Ethereum |
| 5 | Checkpoints keyed on target+network+source | Not on `(chain, wallet)` | Supports multiple target types and ingestion sources |
| 6 | Additive migration path | New tables added alongside old; no destructive changes | Wallet flows must keep working |
| 7 | Dual-write during transition | Repository writes both old and new tables | Zero downtime for existing consumers |
| 8 | Connector trait parameterized by target | `Connector::backfill(target, checkpoint, limit)` | Replaces wallet-only `ChainIngestor::fetch_history` |
| 9 | Ledger as one Silver dataset | `ledger_entries` stays but is not the only normalized output | Platform must support non-ledger analytics |
| 10 | Gold is derived, not foundational | Materialized views built from Silver | No Gold-layer tables in the core schema |

---

## 2. Canonical Raw Data Model — Bronze (RFC Section 3.1)

Bronze stores immutable, canonical chain facts. Raw data is target-agnostic: it
does not carry consumer identity (`user_id`) or a single tracked subject
(`wallet_address`).

### Bronze Tables

| Table | Identity | Content |
|---|---|---|
| `raw_transactions` | `(network, tx_hash)` unique | Raw transaction envelope: hash, timestamp, block reference, chain-specific metadata as JSONB. No `wallet_address`, no `user_id`. |
| `raw_blocks` | `(network, block_num)` unique | Block headers for finality tracking and reorg detection. |
| `raw_evm_logs` | FK to `raw_transactions` | Individual EVM log entries with address, topics, data. Existing `evm_logs` table migrates here. |
| `raw_solana_instructions` | FK to `raw_transactions` | Decoded or raw instruction data with program ID, accounts, data payload. |
| `raw_hl_events` | FK to `raw_transactions` | Hyperliquid event payloads (fills, funding, ledger updates). Existing `hl_fills` partially covers this. |

### Bronze Design Rules

1. **No consumer identity in Bronze.** `user_id` does not belong here.
2. **No single-target ownership.** `wallet_address` does not belong on raw transaction rows.
3. **One row per on-chain fact.** The same transaction hash on the same network produces exactly one `raw_transactions` row, regardless of how many targets reference it.
4. **JSONB for chain-specific data.** The `raw_metadata` column carries everything the chain provides. Typed extension tables extract the most useful fields for indexed access.
5. **Ingestion metadata.** Every raw row carries `ingested_at` (wall clock), `ingestion_run_id` (FK to `ingestion_runs`), and `source` (rpc, grpc, rest, ws) for provenance.

---

## 3. Target Matching Model (RFC Section 3.2)

Targets are the V2 replacement for the wallet-as-organizing-principle pattern.

### `index_targets` Table

```
index_targets
  id            UUID PRIMARY KEY
  kind          target_kind_enum NOT NULL   -- wallet, contract, program, account, topic_filter, market, pool, protocol
  network       TEXT NOT NULL               -- solana-mainnet, ethereum-mainnet, base-mainnet, etc.
  chain_family  chain_family_enum NOT NULL  -- solana, evm, hyperliquid
  address       TEXT                        -- primary address for simple target types
  filter_spec   JSONB                       -- structured filter for complex targets
  mode          target_mode_enum NOT NULL   -- backfill, stream, both
  label         TEXT                        -- human-readable label
  owner_id      UUID                        -- optional tenant/user association
  created_at    TIMESTAMPTZ DEFAULT NOW()
  updated_at    TIMESTAMPTZ DEFAULT NOW()
```

### Target Kinds

- **wallet**: an EOA, Solana pubkey, or Hyperliquid user address; ingestion fetches activity involving this address as a signer, sender, or receiver
- **contract**: an EVM contract address; ingestion fetches logs emitted by this contract
- **program**: a Solana program ID; ingestion fetches transactions that invoke this program
- **account**: a specific on-chain account (e.g. a Solana PDA, token account, or arbitrary account pubkey); ingestion fetches transactions that read or write this account. Distinct from `wallet` because an account target does not imply signer/owner semantics
- **topic_filter**: an EVM topic filter; ingestion fetches logs matching topic criteria
- **market**: a Hyperliquid market symbol; ingestion fetches fills/funding for this market
- **pool**: a DeFi pool address; ingestion fetches pool-related events
- **protocol**: a named protocol; ingestion fetches activity across the protocol's known contracts/programs

The `account` vs `wallet` distinction matters because on Solana, a program-derived
address (PDA) is not a wallet but is a valid indexing target. On EVM chains,
`account` is less useful because the protocol does not distinguish wallet addresses
from contract addresses; use `wallet`, `contract`, or `topic_filter` instead.
P0-W2 defines the per-chain-family validity matrix.

### `target_matches` Table

```
target_matches
  id                UUID PRIMARY KEY
  target_id         UUID NOT NULL REFERENCES index_targets(id)
  raw_transaction_id UUID NOT NULL REFERENCES raw_transactions(id)
  match_reason      TEXT           -- e.g. "sender", "receiver", "log_emitter", "account_key", "program_id"
  matched_at        TIMESTAMPTZ DEFAULT NOW()

  UNIQUE (target_id, raw_transaction_id)
```

This solves the current structural limitation: the same raw transaction can now be
linked to multiple targets.

---

## 4. Network and Chain Family Model (RFC Section 3.3)

**Cross-reference: P0-W3 (Network Model Spec) provides the full network taxonomy,
finality label refinements, checkpoint cursor shapes, and worked examples. This
section captures the RFC-level contract; P0-W3 is the authoritative network model
reference for Phase 1.**

The current `Chain` enum (`Solana`, `Hyperliquid`, `Ethereum` at
`core/src/models.rs` lines 6-10) conflates chain families with specific networks.

### V2 Model

```
chain_family_enum: solana, evm, hyperliquid

networks
  id              TEXT PRIMARY KEY       -- e.g. "solana-mainnet", "ethereum-mainnet", "base-mainnet"
  chain_family    chain_family_enum NOT NULL
  display_name    TEXT NOT NULL
  is_testnet      BOOLEAN NOT NULL DEFAULT FALSE
  finality_model  TEXT NOT NULL          -- see P0-W3 for the four finality labels
  block_time_ms   INTEGER               -- typical block/slot time in milliseconds; NULL for blockless chains
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
```

RPC URL configuration is excluded from this table. RPC endpoints are
deployment-specific and belong in `spectraplex.toml` or environment variables.

P0-W3 refines the finality labels to four values: `probabilistic-slot`,
`probabilistic-block`, `deterministic-block`, `instant`. The key decision locked
here: the `chain` column in raw tables and checkpoints becomes `network TEXT`
referencing the networks table, while `chain_family` is derivable from the network.

---

## 5. Control Plane (RFC Section 3.4)

### `ingestion_runs`

Every ingestion operation (backfill batch, stream session, gap fill) creates a run
record:

```
ingestion_runs
  id              UUID PRIMARY KEY
  target_id       UUID REFERENCES index_targets(id)
  network         TEXT NOT NULL
  source          TEXT NOT NULL         -- "rpc", "grpc", "rest", "ws"
  mode            TEXT NOT NULL         -- "backfill", "stream", "gap_fill"
  status          TEXT NOT NULL         -- "running", "completed", "failed", "cancelled"
  started_at      TIMESTAMPTZ NOT NULL
  finished_at     TIMESTAMPTZ
  records_written BIGINT DEFAULT 0
  error_message   TEXT
  cursor_state    JSONB                 -- checkpoint data specific to this run
```

### `checkpoints` (Replaces `indexer_checkpoints`)

The current `indexer_checkpoints` table is keyed on `(chain, wallet_address)`
(`20260217010000_add_indexer_checkpoints.sql` line 8). V2 checkpoints are keyed on
`(target_id, network, source)`:

```
checkpoints
  id              UUID PRIMARY KEY
  target_id       UUID NOT NULL REFERENCES index_targets(id)
  network         TEXT NOT NULL
  source          TEXT NOT NULL         -- "rpc", "grpc", "rest", "ws"
  cursor          JSONB NOT NULL        -- chain-specific cursor
  updated_at      TIMESTAMPTZ DEFAULT NOW()

  UNIQUE (target_id, network, source)
```

Cursor is JSONB because different chain families have different checkpoint semantics:
- Solana: `{ last_signature, last_slot }`
- EVM: `{ last_block, last_block_hash }`
- Hyperliquid: `{ last_timestamp, last_fill_tid }`

P0-W3 extends these with additional fields (e.g. `commitment` for Solana,
`last_timestamp_ms` for Hyperliquid). The authoritative cursor field definitions
for Phase 1 are in the P0-W3 handoff.

### `dataset_versions`

Tracks which parser/materializer version produced a given normalized dataset:

```
dataset_versions
  id              UUID PRIMARY KEY
  dataset_name    TEXT NOT NULL         -- "ledger_entries", "token_transfers", "decoded_events", etc.
  version         INT NOT NULL
  parser_hash     TEXT                  -- hash of the parser code/config that produced this version
  created_at      TIMESTAMPTZ NOT NULL
  notes           TEXT
```

---

## 6. Silver Dataset Catalog (RFC Section 3.5)

Silver datasets are reusable normalized extractions from Bronze. The current
`ledger_entries` table is the first Silver dataset. V2 expands Silver:

| Dataset | Description | Derived From |
|---|---|---|
| `ledger_entries` | Financial ledger for tax/portfolio (existing) | All raw transactions via chain-specific parsers |
| `token_transfers` | Canonical token transfer records | EVM ERC-20 Transfer logs, Solana token balance deltas, HL transfers |
| `native_balance_deltas` | Native currency changes per account per tx | Pre/post balance diffs |
| `decoded_events` | ABI-decoded EVM events and Solana instruction logs | `raw_evm_logs`, `raw_solana_instructions` |
| `hl_fills` | Hyperliquid fill records (already exists) | `raw_hl_events` |
| `hl_funding` | Hyperliquid funding payments | `raw_hl_events` |
| `positions` | Position state changes | Fills, funding, liquidations |

Each Silver dataset:
- Has a `dataset_version_id` linking to the parser version that produced it
- Can be regenerated from Bronze at any time
- Can be queried by target via `target_matches` joins

---

## 7. Gold Materialization Model (RFC Section 3.6)

Gold layer contains derived, consumer-specific views built from Silver datasets.
These are not core tables but query-time or materialized aggregations:

| View | Built From | Consumer |
|---|---|---|
| Wallet balance history | `token_transfers` + `native_balance_deltas` | Portfolio dashboards |
| Tax lot report | `ledger_entries` + price data | Tax software |
| Protocol TVL snapshot | `decoded_events` + pool state | Analytics dashboards |
| Trader PnL summary | `hl_fills` + `hl_funding` | Trading analytics |

Gold is the province of downstream materializations and reference packs (Phase 5).
The V2 architecture does not prescribe Gold table shapes, only that they derive
from Silver and are not treated as source-of-truth data.

---

## 8. V2 Connector Trait (RFC Section 4.2)

The V2 connector is parameterized by target spec, not wallet string:

```rust
#[async_trait]
pub trait Connector {
    /// Backfill historical data for the given target.
    async fn backfill(
        &self,
        target: &IndexTarget,
        checkpoint: Option<&Checkpoint>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch>;

    /// Open a streaming subscription for the given target.
    async fn stream(
        &self,
        target: &IndexTarget,
        checkpoint: Option<&Checkpoint>,
    ) -> anyhow::Result<(mpsc::Receiver<RawRecord>, JoinHandle<()>)>;

    /// Which target kinds does this connector support?
    fn supported_target_kinds(&self) -> &[TargetKind];

    /// Which network does this connector serve?
    fn network(&self) -> &str;
}
```

Where:
- `IndexTarget` carries the target kind, address, filter spec, and mode
- `Checkpoint` is the V2 JSONB cursor structure
- `IngestionBatch` contains `Vec<RawRecord>`, an updated checkpoint, and run metadata
- `RawRecord` is the canonical raw data struct without `user_id` or `wallet_address`

### Connector Per Chain Family (RFC Section 4.3)

| Chain Family | Connector | Backfill Source | Stream Source | Supported Target Kinds |
|---|---|---|---|---|
| Solana | `SolanaRpcConnector` | `getSignaturesForAddress` / `getTransaction` | (not supported) | wallet, program, account |
| Solana | `SolanaGrpcConnector` | (limited; use RPC for deep backfill) | Yellowstone gRPC | program, account, wallet (via account filtering) |
| EVM | `EvmConnector` | `eth_getLogs`, `eth_getBlockByNumber`, traces | `eth_subscribe` (future) | contract, wallet, topic_filter |
| Hyperliquid | `HyperliquidRestConnector` | REST API (fills, funding, ledger) | (not supported) | wallet (user), market |
| Hyperliquid | `HyperliquidWsConnector` | (not supported) | WebSocket subscriptions | wallet (user), market |

---

## 9. Documented Wallet-Centric Mismatches (RFC Section 2)

Every wallet-centric assumption in the current codebase, verified with exact
file and line references against the codebase at commit `c1752ff`.

### 9.1 Core Types (`core/src/models.rs`)

| Element | Wallet Assumption | Line |
|---|---|---|
| `Transaction.user_id` | Consumer identity baked into raw Bronze data | line 25 |
| `Transaction.wallet_address` | Single wallet stamped on every raw record | line 26 |
| `IndexerCheckpoint` | Keyed on `(chain, wallet_address)` | lines 47-54 |
| `ChainIngestor::fetch_history` | Takes `wallet: &str` and `user_id: Uuid` as required inputs | lines 57-64 |
| `Chain` enum | Three flat variants: `Solana`, `Hyperliquid`, `Ethereum` | lines 6-10 |

### 9.2 Schema / Migrations

| Table | Wallet Assumption | Migration |
|---|---|---|
| `transactions` | Columns `user_id UUID NOT NULL` (line 8) and `wallet_address VARCHAR(255) NOT NULL` (line 9); indexed on `(wallet_address, timestamp)` (line 18) | `20251219000000_init.sql` |
| `transactions` | Unique constraint on `(chain, tx_hash)` but each row stores one `wallet_address` and one `user_id` | `20260217100000_add_unique_chain_tx_hash.sql` line 5 |
| `ledger_entries` | Column `user_id UUID NOT NULL` (line 26) | `20251219000000_init.sql` |
| `ledger_entries` | Column `wallet_address` added | `20251219010000_add_wallet_to_ledger.sql` |
| `indexer_checkpoints` | Primary key `(chain, wallet_address)` (line 8) | `20260217010000_add_indexer_checkpoints.sql` |

The `(chain, tx_hash)` uniqueness constraint on `transactions` combined with the
single `wallet_address` column means the same raw transaction cannot be associated
with more than one tracked wallet.

### 9.3 Repository Layer (`adapters/src/repo.rs`)

**Read methods (all wallet-scoped):**

| Method | Wallet Assumption | Line |
|---|---|---|
| `get_transactions_by_wallet` | Queries `WHERE wallet_address = $1` | line 201 |
| `get_transactions_by_wallet_paginated` | Delegates to `get_transactions_by_wallet_filtered` | line 209 |
| `get_transactions_by_wallet_filtered` | Queries `WHERE wallet_address = $1` with optional date range | line 219 |
| `get_ledger_entries_by_wallet` | Queries `WHERE l.wallet_address = $1` | line 270 |
| `get_ledger_entries_by_wallet_paginated` | Delegates to `get_ledger_entries_by_wallet_filtered` | line 278 |
| `get_ledger_entries_by_wallet_filtered` | Queries `WHERE l.wallet_address = $1` with optional date range | line 288 |
| `get_balances` | Queries `WHERE wallet_address = $1` with GROUP BY asset | line 344 |
| `get_wallet_stats` | Queries `WHERE wallet_address = $1` across transactions and ledger_entries | line 463 |
| `get_checkpoint` | Queries `WHERE chain = $1 AND wallet_address = $2` | line 394 |
| `get_transaction_by_hash` | Queries `WHERE wallet_address = $1 AND tx_hash = $2` | line 428 |

**Write methods (embed `wallet_address` and `user_id` into every inserted row):**

| Method | Wallet Assumption | Line |
|---|---|---|
| `save_transactions` / `build_transaction_insert` | Inserts `wallet_address` and `user_id` into every row | lines 102-146 |
| `save_ledger_entries` / `build_ledger_insert` | Inserts `wallet_address` and `user_id` into every row | lines 148-199 |
| `save_checkpoint` / `save_checkpoint_with` | Inserts `wallet_address` as part of the primary key | lines 526-562 |
| `build_checkpoint` | Takes `wallet: &str` parameter, stamps `wallet_address` on checkpoint | lines 42-78 |
| `save_transactions_with` | Same as `save_transactions`; inserts `wallet_address` and `user_id` | lines 564-605 |
| `save_transactions_and_checkpoint` | Combines transaction and checkpoint writes, both wallet-scoped | lines 607-621 |

### 9.4 API (`api/src/main.rs`)

**Data endpoints (all wallet-parameterized, routes at lines 178-194):**

| Route | Handler | Handler Line |
|---|---|---|
| `GET /v1/transactions/{wallet}` | `get_transactions` | line 744 |
| `GET /v1/transactions/{wallet}/{tx_hash}` | `get_single_transaction` | line 886 |
| `GET /v1/ledger/{wallet}` | `get_ledger` | line 762 |
| `GET /v1/export/{wallet}` | `export_ledger` | line 800 |
| `GET /v1/balances/{wallet}` | `get_balances` | line 864 |
| `GET /v1/stats/{wallet}` | `get_wallet_stats` | line 905 |

**Ingestion endpoints (require a `wallet` field):**

| Endpoint | Request Struct | Struct Line | Handler Line |
|---|---|---|---|
| `POST /v1/ingest` | `IngestRequest { chain, wallet, user_id?, callback_url? }` | lines 256-261 | line 414 |
| `POST /v1/ingest/batch` | `BatchIngestRequest { wallets: Vec<IngestRequest> }` | lines 263-266 | line 561 |
| `POST /v1/normalize` | `NormalizeRequest { wallet, callback_url? }` | lines 268-272 | line 612 |

**Access control and validation:**

| Element | Description | Line |
|---|---|---|
| `AppState.allowed_wallets` | `Option<HashSet<String>>` for wallet allowlisting | line 93 |
| `validate_wallet` | Input validation scoped to wallet address format | line 337 |
| `check_wallet_allowed` | Guards endpoints against non-allowlisted wallets | line 405 |

### 9.5 CLI (`cli/src/main.rs`)

| Element | Wallet Assumption | Line |
|---|---|---|
| `--wallet` argument | Required for `ingest`; `num_args = 1..` for multi-wallet | lines 41-42 |
| `--user-id` argument | Optional consumer identity baked into ingested data | lines 60-61 |
| Checkpoint lookup per wallet | `repo.get_checkpoint(&chain, wallet)` in iteration loop | lines 121-143 |
| `fetch_history` calls | Every chain adapter called with `wallet` and `user_id` | lines 155, 160, 169, 178 |
| `build_checkpoint` call | Takes `&chain, wallet` | line 189 |
| Normalize `db:{wallet}` | Reads transactions from DB filtered by wallet prefix | lines 221-228 |

---

## 10. Chain-Specific Semantic Corrections (RFC Section 6)

### 10.1 EVM `eth_getLogs` Address Semantics

**Current behavior** (`adapters/src/evm.rs`): `fetch_history` (line 115) parses
the wallet as `Address` (line 122) and passes it to `fetch_logs` (line 134).
Inside `fetch_logs` (line 61), the address is used in
`Filter::new().address(address)` (lines 75-78). This filters logs by *emitting
contract address*, not by involved wallet. An EOA wallet address will never appear
as a log emitter, returning zero useful results for wallet history.

**Required correction (P2-W3):**
- **Wallet target**: Filter ERC-20 Transfer events by `from`/`to` indexed parameters. For native ETH transfers, use block-level transaction scanning or trace APIs.
- **Contract target**: Use `eth_getLogs` with the contract address. This is correct.
- **Topic filter target**: Use `eth_getLogs` with user-specified topic filters.

### 10.2 Solana gRPC Program Filter vs Tracked Target Mismatch

**Current behavior** (`adapters/src/solana_grpc.rs`): The gRPC subscription filter
(`build_subscribe_request`, line 115) uses `account_include` set to
`self.config.program_ids` (line 122), defaulting to System Program, Token Program,
and Associated Token Program (`DEFAULT_PROGRAM_IDS`, lines 20-24). `fetch_history`
(line 286) stamps the caller's wallet onto every matching transaction (lines
323-324), even though the subscription has no relationship to the requested wallet.

**Required correction (P2-W4):**
- **Program target**: Subscribe with `account_include = [program_id]`. Do not stamp any wallet.
- **Wallet target**: Subscribe with account filters that include the wallet's pubkey.
- **Account target**: Subscribe with the specific account pubkey in `account_include`.

### 10.3 Hyperliquid User vs Market Semantics

**Current behavior** (`adapters/src/hyperliquid.rs`): `fetch_history` (line 171)
uses the wallet parameter as a HyperCore user address for fills (line 189),
funding (line 208), and ledger updates (line 227). This is correct for
user-scoped queries.

**Required correction (P2-W5):**
- **Wallet (user) target**: Continue using user-scoped REST APIs. Correct.
- **Market target**: Use market-scoped APIs. New capability.
- **HyperEVM target**: Route to the EVM connector with the HyperEVM network.

### 10.4 Chain Enum Flattening

**Current behavior**: `Chain::Ethereum` (`core/src/models.rs` lines 6-10) is used
for all EVM chains. The repository maps it via `chain_to_str`/`str_to_chain`
(`adapters/src/repo.rs` lines 4-19).

**Required correction (P0-W3, P1-W1):** Replace `Chain` with
`(ChainFamily, Network)` pairs. `ChainFamily::Evm` covers Ethereum, Base,
Arbitrum, Optimism, HyperEVM, etc. Connector instantiation takes a network ID.

---

## 11. Dual-Write Compatibility Strategy (RFC Section 5)

### 11.1 Core Principle

The transition is additive, not destructive. New tables and types are added
alongside existing ones. Existing wallet-facing API and CLI flows continue to work
during the transition.

### 11.2 Dual-Write Boundary

During the transition period (Phase 1 through Phase 2), ingestion writes to both
old and new tables:

```
Ingestion request (wallet-shaped)
  |
  |-- [compatibility shim] create an IndexTarget of kind=wallet if not exists
  |-- [compatibility shim] use old ChainIngestor::fetch_history
  |
  +-- write to `transactions` (old Bronze, unchanged)
  +-- write to `raw_transactions` (new canonical Bronze)
  +-- write to `target_matches` (link new raw record to the wallet target)
  +-- write to `checkpoints` (new control plane)
  +-- write to `indexer_checkpoints` (old control plane, for backward compat)
```

The dual-write happens in the repository layer. Adapters continue to produce the
same `Transaction` struct during transition.

### 11.3 Dual-Read Boundary

**Phase 1 (immediate):** Existing wallet-scoped reads continue to query old tables.
No read-path changes.

**Phase 2 (after V2 writes are stable):** Wallet-scoped reads are redirected to
query through `target_matches` + `raw_transactions`. The old `transactions` table
becomes a write-only compatibility sink.

### 11.4 Schema Migration Sequence

All migrations are additive:

1. **P1-W2**: Add `chain_family_enum`, `target_kind_enum`, `target_mode_enum` types. Add `networks`, `index_targets`, `ingestion_runs`, `checkpoints`, `target_matches`, `dataset_versions` tables. Add `raw_transactions` table. **Do not alter existing tables.**
2. **P1-W4**: Add dual-write logic in the repository layer.
3. **P2-W2**: Add new API endpoints for target registration and target-scoped queries.
4. **Eventually**: Deprecate `transactions.wallet_address`, `transactions.user_id`, and `indexer_checkpoints`.

### 11.5 What Stays Unchanged During Transition

- `ledger_entries` table and all its wallet-scoped queries
- All current API endpoints and their response shapes
- All current CLI commands and their flags
- Extension tables: `blocks`, `evm_logs`, `hl_fills`
- Authentication and authorization model

---

## 12. Deprecation Sequence (RFC Section 5.5)

| Assumption | Deprecation Phase | Replacement |
|---|---|---|
| `Transaction.user_id` in core struct (`core/src/models.rs` line 25) | Phase 1 (new `RawRecord` struct added; old struct kept) | Target ownership via `index_targets.owner_id` |
| `Transaction.wallet_address` in core struct (`core/src/models.rs` line 26) | Phase 1 (new struct added; old struct kept) | Target linkage via `target_matches` |
| `transactions.wallet_address` column (`20251219000000_init.sql` line 9) | Phase 2+ (reads migrated) | `target_matches` join |
| `transactions.user_id` column (`20251219000000_init.sql` line 8) | Phase 2+ (reads migrated) | `index_targets.owner_id` |
| `indexer_checkpoints` table (`20260217010000_add_indexer_checkpoints.sql`) | Phase 2+ (writes migrated) | `checkpoints` table |
| `ChainIngestor::fetch_history` trait (`core/src/models.rs` lines 57-64) | Phase 2 (new `Connector` trait added; old trait kept) | `Connector::backfill` |
| Wallet-only API endpoints (`api/src/main.rs` lines 178-194) | Phase 4+ (new dataset endpoints added) | Target-scoped and dataset-scoped endpoints |
| Wallet-only CLI flags (`cli/src/main.rs` lines 41-42) | Phase 4+ (new target flags added) | Target-scoped CLI flags |

---

## 13. Downstream Packet Mapping (RFC Section 9)

| Phase | Work Packets | What This RFC Locks For Them |
|---|---|---|
| **P0-W2** (Target Model) | Target kinds, filter_spec shape, ownership rules | Section 3.2 defines the `index_targets` table shape and locked target kinds (including `account`). P0-W2 fills in per-kind semantics, per-chain-family validity, and filter_spec details. |
| **P0-W3** (Network Model) | Chain family enum, network taxonomy, finality semantics | Section 3.3 defines the split and the `networks` table. P0-W3 fills in the full taxonomy. **P0-W3 handoff verified at `docs/phase0/p0-w3-network-model-handoff.md`; its Section 8.1 confirms alignment with this RFC.** |
| **P0-W4** (Rollout Plan) | Migration ordering, dual-write mechanics, test matrix | Section 5 defines the compatibility strategy. P0-W4 produces the detailed execution checklist and test plan. |
| P1-W1 (Core types) | New Rust types | Sections 3.1, 3.2, 3.4 define the domain model. |
| P1-W2 (Migrations) | Table definitions | Sections 3.1-3.4 define all new tables. |
| P1-W3 (Repo layer) | Repository methods | Section 3.2 (target_matches), Section 3.4 (checkpoints, runs) define the query shapes. |
| P1-W4 (Dual-write) | Compatibility boundary | Section 5.2 defines the dual-write strategy. |
| P2-W1 (Connector) | New trait signature | Section 4.2 defines the `Connector` trait. |
| P2-W3 (EVM fix) | Correct EVM semantics | Section 6.1 documents the problem and required fix. |
| P2-W4 (Solana fix) | Correct gRPC semantics | Section 6.2 documents the problem and required fix. |
| P2-W5 (HL targets) | Hyperliquid target model | Section 6.3 documents the required additions. |
| P3-W1+ (Silver) | Dataset list and versioning | Section 3.5 defines the Silver dataset catalog. |

---

## 14. Open Questions Deferred to Follow-On Packets (RFC Section 8)

| Question | Deferred To |
|---|---|
| Exact `filter_spec` JSONB shape per target kind | P0-W2 |
| Per-chain-family validity matrix for target kinds | P0-W2 |
| Full network taxonomy and finality model details | P0-W3 |
| Detailed test matrix for dual-write transition | P0-W4 |
| Exact column types and indexes for `raw_transactions` | P1-W2 |
| Whether `raw_solana_instructions` is populated eagerly or lazily | P1-W2 / P2-W4 |
| Reorg handling model for EVM Bronze data | P2-W3 |
| Parser versioning and regeneration workflow | P3-W1 |
| Dataset-oriented API endpoint design | P4-W1 |
| Price service integration | Phase 5 |

---

## 15. Verification Notes

All file and line references in this handoff were verified against the codebase at
commit `c1752ff` (tip of `main` as of 2026-03-08). No runtime code was changed by
this work packet.

Verification commands run:
- `cargo fmt --all --check` — passed
- `cargo clippy --workspace --all-targets -- -D warnings` — passed
- `cargo test --workspace` — passed
