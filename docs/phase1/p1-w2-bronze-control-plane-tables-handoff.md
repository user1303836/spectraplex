# P1-W2: Add New Bronze And Control-Plane Tables — Handoff

**Phase:** Phase 1 – Canonical Bronze And Control Plane
**Status:** Complete
**Branch:** `p1-w2-bronze-control-plane-tables`
**Date:** 2026-03-08

## Objective

Create additive SQL migrations for all V2 tables with correct enum types, indexes, uniqueness constraints, and seed data for 13 networks. Zero changes to existing V1 tables.

## Migrations Added

### `20260308000000_add_v2_enums.sql`

Three new PostgreSQL enum types:

| Type | Values | Rust match |
|---|---|---|
| `chain_family_enum` | `solana`, `evm`, `hyperliquid` | `ChainFamily` (serde `rename_all = "lowercase"`) |
| `target_kind_enum` | `wallet`, `contract`, `program`, `account`, `topic_filter`, `market`, `pool`, `protocol` | `TargetKind` (serde `rename_all = "snake_case"`) |
| `target_mode_enum` | `backfill`, `stream`, `both` | `TargetMode` (serde `rename_all = "snake_case"`) |

Existing V1 enums (`chain_enum`, `entry_type_enum`) are untouched.

### `20260308000001_add_networks.sql`

- `networks` table with TEXT primary key, `chain_family`, `display_name`, `is_testnet`, `finality_model`, `block_time_ms`, `created_at`.
- Seeded with 13 canonical networks per P0-W3 Section 3.3:
  - **Solana (3):** `solana-mainnet`, `solana-devnet`, `solana-testnet`
  - **EVM (8):** `ethereum-mainnet`, `ethereum-sepolia`, `base-mainnet`, `base-sepolia`, `arbitrum-mainnet`, `arbitrum-sepolia`, `hyperevm-mainnet`, `hyperevm-testnet`
  - **Hyperliquid (2):** `hypercore-mainnet`, `hypercore-testnet`

### `20260308000002_add_v2_tables.sql`

Six new tables:

| Table | Purpose | Key constraints |
|---|---|---|
| `index_targets` | Registered indexing subjects | Partial unique on `(kind, network, address)` WHERE address IS NOT NULL; partial unique on `(kind, network, filter_spec_hash)` WHERE filter_spec_hash IS NOT NULL |
| `raw_transactions` | Canonical bronze layer (no `wallet_address`, no `user_id`) | UNIQUE `(network, tx_hash)` |
| `target_matches` | Join table: raw transactions ↔ index targets | UNIQUE `(target_id, raw_transaction_id)` |
| `ingestion_runs` | Control-plane ingestion operation records | FK to `index_targets` |
| `checkpoints` | V2 resumption cursors | UNIQUE `(target_id, network, source)` |
| `dataset_versions` | Parser/materializer version tracking | — |

## Indexes Added

- `idx_index_targets_network` — `index_targets(network)`
- `idx_index_targets_kind_network` — `index_targets(kind, network)`
- `idx_raw_transactions_network_hash` — `raw_transactions(network, tx_hash)`
- `idx_raw_transactions_timestamp` — `raw_transactions(timestamp)`
- `idx_raw_transactions_run` — `raw_transactions(ingestion_run_id)`
- `idx_target_matches_target` — `target_matches(target_id)`
- `idx_target_matches_raw_tx` — `target_matches(raw_transaction_id)`
- `idx_ingestion_runs_target` — `ingestion_runs(target_id)`
- `idx_ingestion_runs_status` — `ingestion_runs(status)`

## Foreign Key References

- `index_targets.network` → `networks(id)`
- `raw_transactions.network` → `networks(id)`
- `raw_transactions.ingestion_run_id` → `ingestion_runs(id)`
- `target_matches.target_id` → `index_targets(id)`
- `target_matches.raw_transaction_id` → `raw_transactions(id)`
- `ingestion_runs.target_id` → `index_targets(id)`
- `checkpoints.target_id` → `index_targets(id)`

## V1 Tables Untouched

The following existing tables and types are not altered or dropped:

- `transactions`
- `ledger_entries`
- `indexer_checkpoints`
- `blocks`
- `evm_logs`
- `hl_fills`
- `chain_enum`
- `entry_type_enum`

## Design References

- V2 Architecture RFC (P0-W1) — Sections 3.1–3.5
- Target Model Spec (P0-W2) — Section 6 (uniqueness constraints)
- Network Model Spec (P0-W3) — Section 3.1 (schema), 3.3 (seed data)
- V2 Core Types (P1-W1) — `core/src/v2.rs` enum serde formats

## Verification

- `cargo fmt --all --check` — pass
- `cargo clippy --workspace --all-targets -- -D warnings` — pass
- `cargo test --workspace` — pass (all existing tests, 0 failures)
- `cargo build --release --workspace` — pass

## Follow-ups for P1-W3+

- Add `sqlx` compile-time query support for V2 tables in the repository layer
- Wire up V2 checkpoint persistence in adapters
- Implement dual-write path bridging V1 → V2 bronze tables
