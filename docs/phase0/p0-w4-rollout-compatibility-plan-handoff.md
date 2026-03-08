# P0-W4 Rollout And Compatibility Plan Handoff

Status: **Frozen**
Date: 2026-03-08
Phase: P0-W4
Branch: `codex/p0-w4-rollout-compatibility-plan`
Depends on: P0-W1 (V2 Architecture RFC), P0-W2 (Target Model Spec), P0-W3 (Network Model)
Downstream dependents: All Phase 1+ work packets

This document is the tracked, reviewable artifact for the P0-W4 Rollout And
Compatibility Plan. It captures the locked decisions from the full plan
(`design/ROLLOUT_AND_COMPATIBILITY_PLAN.md`, which lives in the gitignored
`design/` directory) so that reviewers can validate the rollout contract on
GitHub without accessing local-only design docs.

No new scope is introduced here. Every item below is drawn directly from the
frozen `design/ROLLOUT_AND_COMPATIBILITY_PLAN.md`.

---

## 1. Compatibility Window

### 1.1 Window Boundaries

| Milestone | Phase | Description |
|---|---|---|
| **Window opens** | P1-W2 | First V2 migration lands (new tables alongside old) |
| **Dual-write active** | P1-W4 | Repository writes to both V1 and V2 tables |
| **Dual-read available** | P2-W2 | New target-scoped read paths exist; V1 reads still primary |
| **V1 reads redirected** | P2-W6 | Wallet-scoped reads optionally route through V2 tables |
| **V1 writes deprecated** | Phase 3+ | V1 tables become read-only compatibility views |
| **Window closes** | Phase 4+ | V1 tables can be dropped (requires explicit user approval) |

### 1.2 Invariants

These must hold true for the entire duration of the compatibility window:

1. `cargo test --workspace` passes at every commit
2. `cargo fmt --all --check` passes at every commit
3. `cargo clippy --workspace --all-targets -- -D warnings` passes at every commit
4. Existing wallet API endpoints return identical response shapes
5. Existing CLI commands accept the same flags and produce the same behavior
6. No data loss during the transition
7. Rollback is possible by removing dual-write and reverting to V1-only paths

---

## 2. Old-To-New Schema Mapping

### 2.1 Table-Level Mapping

| V1 Table | V2 Replacement | Migration Strategy |
|---|---|---|
| `transactions` | `raw_transactions` + `target_matches` | Additive; `transactions` kept during window |
| `ledger_entries` | `ledger_entries` (unchanged initially) | Unchanged in Phase 1-2; derived from Silver in Phase 3 |
| `indexer_checkpoints` | `checkpoints` | Additive; both written during dual-write |
| `blocks` | `raw_blocks` | Rename/migrate in P1-W2 |
| `evm_logs` | `raw_evm_logs` | Rename/migrate in P1-W2 |
| `hl_fills` | `raw_hl_events` (superset) | Additive; `hl_fills` kept initially |
| (none) | `networks` | New table, seeded in P1-W2 |
| (none) | `index_targets` | New table |
| (none) | `target_matches` | New table |
| (none) | `ingestion_runs` | New table |
| (none) | `dataset_versions` | New table |

### 2.2 Column Mapping: `transactions` → `raw_transactions`

| V1 Column | V2 Column | Notes |
|---|---|---|
| `id` (UUID PK) | `id` (UUID PK) | Direct mapping |
| `user_id` (UUID NOT NULL) | (removed) | Replaced by `index_targets.owner_id` |
| `wallet_address` (VARCHAR NOT NULL) | (removed) | Replaced by `target_matches` join |
| `timestamp` (BIGINT) | `timestamp` (BIGINT) | Direct mapping |
| `tx_hash` (VARCHAR NOT NULL) | `tx_hash` (TEXT NOT NULL) | Direct mapping |
| `chain` (chain_enum) | `network` (TEXT, FK) | Mapped per Section 2.4 |
| `raw_metadata` (JSONB) | `raw_metadata` (JSONB) | Direct mapping |
| `created_at` | `ingested_at` | Rename for clarity |
| (none) | `ingestion_run_id` (UUID, FK) | New provenance field |
| (none) | `source` (TEXT) | New: "rpc", "grpc", "rest", "ws" |
| (none) | `block_number` (BIGINT) | New: extracted for indexed access |

### 2.3 Column Mapping: `indexer_checkpoints` → `checkpoints`

| V1 Column | V2 Column | Notes |
|---|---|---|
| `chain` (chain_enum) | (removed, derived from network) | |
| `wallet_address` (TEXT) | `target_id` (UUID, FK) | Wallet becomes an index target |
| `last_signature` | `cursor.last_signature` | Nested in JSONB cursor |
| `last_slot` | `cursor.last_slot` | Nested in JSONB cursor |
| `last_block` | `cursor.last_block` | Nested in JSONB cursor |
| `last_timestamp` | `cursor.last_timestamp_ms` | Multiply by 1000 (P0-W3 Section 6.3) |
| `updated_at` | `updated_at` | Direct mapping |
| (none) | `network` (TEXT) | New dimension |
| (none) | `source` (TEXT) | New: "rpc", "grpc", "rest", "ws" |

### 2.4 Chain-to-Network Mapping

| V1 `chain` | V2 `chain_family` | V2 default `network` | V2 default `source` |
|---|---|---|---|
| `solana` | `solana` | `solana-mainnet` | `rpc` |
| `hyperliquid` | `hyperliquid` | `hypercore-mainnet` | `rest` |
| `ethereum` | `evm` | `ethereum-mainnet` | `rpc` |

**Caveat (P0-W3 Section 6.4):** Multi-chain EVM deployments using
`Chain::Ethereum` for Base, Arbitrum, etc. require reclassification. The
override mechanism is deferred to P1-W2.

### 2.5 Rust Type Mapping

| V1 Type | V2 Type | Coexistence Strategy |
|---|---|---|
| `Chain` enum | `ChainFamily` + `Network` | Both exist; `Chain` kept for V1 compat |
| `Transaction` struct | `RawRecord` (no `user_id`/`wallet_address`) | Both exist during transition |
| `IndexerCheckpoint` struct | `Checkpoint` (JSONB cursor) | Both exist; repo writes both |
| `ChainIngestor` trait | `Connector` trait | Both exist; `ChainIngestor` wraps `Connector` |

---

## 3. Deprecation Sequence

Consistent with P0-W1 Section 12.

### 3.1 Phase 1 — Soft Deprecation (New Alternatives Added)

| Element | Replacement | Safe To Remove |
|---|---|---|
| `Transaction.user_id` field | `index_targets.owner_id` | Phase 2+ |
| `Transaction.wallet_address` field | `target_matches` join | Phase 2+ |
| `Chain` enum | `ChainFamily` + `Network` | Phase 2+ |
| `ChainIngestor` trait | `Connector` trait | Phase 2+ |

### 3.2 Phase 2 — Active Deprecation (V2 Paths Preferred)

| Element | Status | Replacement | Safe To Remove |
|---|---|---|---|
| `transactions.wallet_address` column | Write-only compat | `target_matches` | Phase 3+ |
| `transactions.user_id` column | Write-only compat | `index_targets.owner_id` | Phase 3+ |
| `indexer_checkpoints` table | Write-only compat | `checkpoints` table | Phase 3+ |
| `ChainIngestor::fetch_history` | Wrapper only | `Connector::backfill` | Phase 3+ |

### 3.3 Phase 3+ — Removal Candidates

| Element | Prerequisite For Removal |
|---|---|
| `transactions` table | All reads migrated to `raw_transactions` + `target_matches` |
| `indexer_checkpoints` table | All reads migrated to `checkpoints` |
| `Chain` enum (Rust) | All code uses `ChainFamily` + `Network` |
| `chain_enum` (SQL) | No tables reference it |
| Wallet-only API routes | Target-scoped routes fully functional |
| Wallet-only CLI flags | Target-scoped flags fully functional |

### 3.4 Elements NOT Deprecated

| Element | Reason |
|---|---|
| `ledger_entries` table | Stays as Silver dataset |
| `entry_type_enum` | Still valid for ledger classification |
| Wallet as a target kind | First-class target kind permanently |
| `/v1/transactions/:wallet` route shape | Remains valid; may route through V2 internally |
| `/v1/ledger/:wallet` route shape | Remains valid |

---

## 4. Per-Packet Compatibility Strategy

Every Phase 1 and Phase 2 work packet has an identified compatibility strategy.

### 4.1 Phase 1 Packets

#### P1-W1: Introduce V2 Core Types

**Strategy:** Additive types only. No existing types modified or removed. New
types (`ChainFamily`, `Network`, `IndexTarget`, `TargetKind`, `RawRecord`,
`Checkpoint`, `IngestionRun`, `Connector` trait) added alongside existing types.

**Unchanged:** `Chain`, `Transaction`, `IndexerCheckpoint`, `ChainIngestor`,
`LedgerEntry`, `EntryType`.

#### P1-W2: Add New Bronze And Control-Plane Tables

**Strategy:** Additive migrations only. New tables created alongside existing
tables. No existing table altered or dropped. New SQL enum types
(`chain_family_enum`, `target_kind_enum`, `target_mode_enum`) created as
separate types, not modifications to `chain_enum`.

**Unchanged:** All existing tables, columns, indexes, constraints, and enum types.

**Migration safety:** Must apply cleanly on empty DB and on top of current V1
schema. No `ALTER TABLE` on existing tables. No `DROP`.

#### P1-W3: Repository Support For Canonical Raw Data

**Strategy:** New repository methods added alongside existing ones. No existing
method signature changed.

**Unchanged:** All wallet-scoped read/write methods.

#### P1-W4: Dual-Write Compatibility Layer

**Strategy:** Repository layer modified so every V1 write also writes to V2
tables. The dual-write boundary is in the repository layer, not in adapters or
handlers.

**Dual-write flow (from P0-W1 Section 11.2):**
```
Ingestion request (wallet-shaped)
  |-- [compat shim] create IndexTarget(kind=wallet) if not exists
  |-- [compat shim] use old ChainIngestor::fetch_history
  |
  +-- write to `transactions` (V1)
  +-- write to `raw_transactions` (V2)
  +-- write to `target_matches` (V2)
  +-- write to `checkpoints` (V2)
  +-- write to `indexer_checkpoints` (V1)
```

**Unchanged:** All API handler signatures/responses. All CLI command
signatures/flags. All adapter `fetch_history` signatures. Read paths continue
to use V1 tables.

**Failure mode:** During early transition, V2 write failure does not block V1
write. Once V2 writes are proven stable, V1 write becomes best-effort.

#### P1-W5: Migration And Rollback Verification

**Strategy:** Verification-only packet. Validates fresh-DB and upgrade-DB
migration paths, cross-target linking, and rollback safety.

### 4.2 Phase 2 Packets

#### P2-W1: Connector Interface Redesign

**Strategy:** New `Connector` trait added alongside `ChainIngestor`. A
compatibility adapter wraps each `ChainIngestor` as a `Connector` that accepts
`IndexTarget(kind=wallet)` and delegates to `fetch_history`.

**Unchanged:** `ChainIngestor` trait and all implementations.

#### P2-W2: CLI And API Target Registration Flow

**Strategy:** New API endpoints and CLI flags added alongside existing ones.
Existing wallet-shaped requests accepted as before and internally create wallet
targets via compatibility shim.

**New endpoints (additive):** `POST /v1/targets`, `GET /v1/targets`,
`GET /v1/targets/:target_id`, `POST /v1/targets/:target_id/ingest`.

**New CLI flags (additive):** `--target-kind`, `--target-address`, `--network`.

**Unchanged:** `POST /v1/ingest`, `POST /v1/ingest/batch`, `--wallet` CLI flag.

#### P2-W3: Fix EVM Ingestion Semantics

**Strategy:** EVM adapter gains target-aware paths. Existing `fetch_history`
preserved as compatibility wrapper. Note: correcting EVM wallet semantics
changes data content (more correct results), not API surface.

**Unchanged:** `fetch_history` method signature.

#### P2-W4: Fix Solana gRPC Target Semantics

**Strategy:** Solana gRPC adapter gains target-aware subscription filters.
Existing `fetch_history` preserved as compatibility wrapper. Stops stamping
arbitrary wallet identity onto program-filtered results.

**Unchanged:** `fetch_history` method signature for backward compatibility.

#### P2-W5: Hyperliquid Target Model

**Strategy:** Hyperliquid adapters gain target-aware paths. Existing wallet/user
path preserved unchanged. Market targets and HyperEVM routing added as new
capabilities.

**Unchanged:** `fetch_history` for wallet/user targets.

#### P2-W6: End-To-End Ingestion Compatibility Suite

**Strategy:** Verification-only packet. Integration tests verify wallet targets
through V1 and V2 paths, new target types through V2 paths, dual-write
consistency, and checkpoint consistency.

---

## 5. Test Matrix

### 5.1 Categories

| Category | Description | When Run |
|---|---|---|
| **Unit** | Rust type tests, serialization, parsing | Every commit |
| **Migration-fresh** | All migrations on empty DB | Migration changes |
| **Migration-upgrade** | New migrations on V1 schema with data | Migration changes |
| **Dual-write** | Both V1 and V2 tables receive writes | P1-W4 onward |
| **Dual-read** | V1 reads match V2 reads | P2-W2 onward |
| **V1-compat** | All existing API/CLI behavior unchanged | Every commit |
| **V2-functional** | New target-scoped paths work | P1-W3 onward |
| **Rollback** | V2 tables droppable without V1 impact | P1-W5 |
| **Cross-target** | One raw tx links to multiple targets | P1-W4 onward |
| **Checkpoint-migration** | Legacy → V2 checkpoint conversion | P1-W4 |

### 5.2 Phase 1 Test Matrix

| Test | P1-W1 | P1-W2 | P1-W3 | P1-W4 | P1-W5 |
|---|---|---|---|---|---|
| Unit (V2 types) | ✓ | | | | |
| Migration-fresh | | ✓ | | | ✓ |
| Migration-upgrade | | ✓ | | | ✓ |
| Repo V2 methods | | | ✓ | | |
| Dual-write | | | | ✓ | ✓ |
| V1-compat (API) | ✓ | ✓ | ✓ | ✓ | ✓ |
| V1-compat (CLI) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Cross-target | | | | ✓ | ✓ |
| Checkpoint-migration | | | | ✓ | ✓ |
| Rollback | | | | | ✓ |

### 5.3 Phase 2 Test Matrix

| Test | P2-W1 | P2-W2 | P2-W3 | P2-W4 | P2-W5 | P2-W6 |
|---|---|---|---|---|---|---|
| Connector wrapper | ✓ | | | | | |
| Target registration | | ✓ | | | | |
| EVM wallet target | | | ✓ | | | ✓ |
| EVM contract target | | | ✓ | | | ✓ |
| Solana wallet target | | | | ✓ | | ✓ |
| Solana program target | | | | ✓ | | ✓ |
| Solana account target | | | | ✓ | | ✓ |
| HL wallet target | | | | | ✓ | ✓ |
| HL market target | | | | | ✓ | ✓ |
| Dual-write | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Dual-read | | ✓ | | | | ✓ |
| V1-compat (full) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |

### 5.4 Dual-Write Test Scenarios

| ID | Scenario | Assertion |
|---|---|---|
| DW-1 | Ingest Solana wallet tx via V1 path | Row in both `transactions` and `raw_transactions`; `target_matches` links wallet target |
| DW-2 | Same tx, two wallets | One `raw_transactions` row, two `target_matches` rows, two `transactions` rows |
| DW-3 | Checkpoint save via V1 path | Row in both `indexer_checkpoints` and `checkpoints` |
| DW-4 | Resume from checkpoint via V1 path | Legacy checkpoint fields produce correct resume |
| DW-5 | V2-only write (new target type) | Row in `raw_transactions` + `target_matches`; no `transactions` row |
| DW-6 | V1 write fails, V2 succeeds | V2 data preserved; V1 failure logged |
| DW-7 | V2 write fails, V1 succeeds | V1 data preserved; V2 failure logged (early transition) |

### 5.5 Dual-Read Test Scenarios

| ID | Scenario | Assertion |
|---|---|---|
| DR-1 | `GET /v1/transactions/:wallet` via V1 | Returns expected transactions |
| DR-2 | `GET /v1/transactions/:wallet` via V2 | Returns same data as DR-1 |
| DR-3 | `GET /v1/ledger/:wallet` via V1 | Returns expected ledger entries |
| DR-4 | `GET /v1/ledger/:wallet` via V2 | Returns same data as DR-3 |
| DR-5 | `GET /v1/balances/:wallet` via V1 | Returns expected balances |
| DR-6 | `GET /v1/balances/:wallet` via V2 | Returns same data as DR-5 |
| DR-7 | `GET /v1/stats/:wallet` via V1 | Returns expected stats |
| DR-8 | `GET /v1/stats/:wallet` via V2 | Returns same data as DR-7 |

### 5.6 Migration Test Scenarios

| ID | Scenario | Assertion |
|---|---|---|
| MG-1 | All migrations on empty PostgreSQL | All tables exist, constraints valid |
| MG-2 | V2 migrations on V1 schema (no data) | Clean application |
| MG-3 | V2 migrations on V1 schema with data | Data preserved, V1 untouched |
| MG-4 | Solana checkpoint migration | `last_signature` → `cursor.last_signature`, `source = "rpc"`, `network = "solana-mainnet"` |
| MG-5 | HyperCore checkpoint migration | `last_timestamp * 1000` → `cursor.last_timestamp_ms`, `source = "rest"`, `network = "hypercore-mainnet"` |
| MG-6 | EVM checkpoint migration | `last_block` → `cursor.last_block`, `source = "rpc"`, `network = "ethereum-mainnet"` |
| MG-7 | Drop V2 tables (rollback) | V1 tables still function, no FK violations |

---

## 6. Compatibility Shim Design

### 6.1 Wallet-to-Target Shim

When a V1 wallet-shaped request arrives:

1. Look up `index_targets` for `(kind=wallet, network=<derived>, address=<wallet>)`
2. If not found, create with `kind = wallet`, `network` from chain mapping
   (Section 2.4), `chain_family` derived from network, `address = <wallet>`,
   `mode = backfill`, `owner_id` from `user_id` if provided
3. Return target ID for V2 write paths

### 6.2 Transaction-to-RawRecord Shim

When adapter produces V1 `Transaction`:

1. Copy `id`, `timestamp`, `tx_hash`, `raw_metadata`
2. Map `chain` → `network` (Section 2.4)
3. Discard `user_id` and `wallet_address` (captured via `target_matches` and
   `index_targets.owner_id`)
4. Add `source` from adapter type

### 6.3 Checkpoint Shim

When repo writes V1 `indexer_checkpoints`:

1. Look up wallet target (Section 6.1)
2. Build JSONB cursor from legacy fields (per P0-W3 Section 6)
3. Set `network` from Section 2.4
4. Set `source` from adapter type

---

## 7. Rollback Strategy

### 7.1 Phase 1 Rollback

- Remove dual-write logic (revert P1-W4)
- V2 tables can be dropped without V1 impact (no FKs from V1 to V2)
- V2 Rust types can be feature-gated or removed
- All V1 paths continue to function

### 7.2 Phase 2 Rollback

- Remove new API endpoints and CLI flags (additive, clean removal)
- Remove connector wrappers; restore direct `ChainIngestor` usage
- Disable dual-write; V1 tables still have all data
- Semantic corrections (EVM, Solana gRPC) evaluated individually

### 7.3 Point of No Return

Rollback becomes impractical once:
- `transactions` table stops receiving writes
- Read paths fully migrated to V2
- V1 tables dropped

This does not happen until Phase 4+ with explicit user approval.

---

## 8. Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Dual-write performance overhead | Medium | Medium | Benchmark; batch V2 writes; same DB transaction |
| V1/V2 data drift | Low | High | Reconciliation tests in P1-W5 and P2-W6 |
| EVM network reclassification | Medium | Medium | P1-W2 defines override mechanism |
| HyperCore timestamp precision loss | Low | Low | Dedup on `(network, tx_hash)` handles re-fetches |
| Legacy gRPC Solana checkpoint source | Low | Low | Default `source = "rpc"` safe for resume |

---

## 9. Decisions Locked By This Plan

| # | Decision | Choice | Rationale |
|---|---|---|---|
| 1 | Additive migration strategy | New tables alongside old; no destructive changes | Wallet flows must keep working |
| 2 | Dual-write boundary is the repository layer | Not adapters or handlers | Single point of dual-write logic; adapters stay simple |
| 3 | V2 write failure does not block V1 write (early) | Best-effort V2 during early transition | De-risks Phase 1 rollout |
| 4 | Compatibility window closes no earlier than Phase 4 | Requires explicit user approval | Maximum safety for existing consumers |
| 5 | Wallet-to-target shim creates IndexTarget automatically | Transparent to V1 callers | Zero disruption for existing API/CLI users |
| 6 | Legacy checkpoints default to `source = "rpc"` | Per P0-W3 Section 6.2/6.5 | Safe for resume; gRPC gets fresh V2 checkpoints |
| 7 | Deprecation sequence matches P0-W1 Section 12 | Phase 1 soft, Phase 2 active, Phase 3+ removal | Gradual transition with clear milestones |
| 8 | Every Phase 1 and Phase 2 packet has explicit compat strategy | Section 4 of this document | No packet starts without a clear compatibility contract |

---

## 10. Cross-References

### 10.1 P0-W1 (V2 Architecture RFC) Dependencies

| P0-W1 Section | Used In This Handoff |
|---|---|
| Section 11 (Dual-Write Strategy) | Sections 2, 4.1 (P1-W4), 5.4, 6 |
| Section 12 (Deprecation Sequence) | Section 3 |
| Section 9 (Wallet-Centric Mismatches) | Sections 2.2, 2.3, 2.5 |
| Section 3.1 (Bronze Design Rules) | Section 2.2, 6.2 |
| Section 3.2 (Target Matching) | Section 6.1 |
| Section 3.4 (Control Plane) | Section 2.3 |

### 10.2 P0-W2 (Target Model Spec) Dependencies

| P0-W2 Section | Used In This Handoff |
|---|---|
| Section 2.1 (wallet kind) | Section 6.1 (wallet-to-target shim) |
| Section 3 (Validity Matrix) | Section 4.2 (per-chain target support) |
| Section 6 (Uniqueness Rules) | Section 6.1 (target dedup) |
| Section 7 (Ownership Rules) | Section 6.1 (owner_id mapping) |
| Section 11 (Downstream Mapping) | Section 4 (per-packet strategies) |

### 10.3 P0-W3 (Network Model) Dependencies

| P0-W3 Section | Used In This Handoff |
|---|---|
| Section 1 (Network Registry) | Section 2.4 (chain-to-network mapping) |
| Section 5 (Cursor Shapes) | Section 2.3 (checkpoint mapping) |
| Section 6 (Legacy Migration) | Sections 2.3, 2.4, 5.6 |
| Section 6.2 (Solana Cursor) | Section 8 (risk: gRPC source) |
| Section 6.3 (HyperCore Timestamp) | Section 8 (risk: timestamp precision) |
| Section 6.4 (EVM Reclassification) | Section 8 (risk: multi-chain EVM) |

---

## 11. Verification Notes

This handoff was verified against the codebase at commit `33205bb` (tip of
`main` as of 2026-03-08). No runtime code was changed by this work packet.

**Completeness checks:**

1. Every P1-W* packet (W1 through W5) has an identified compatibility strategy
   (Section 4.1) — **verified**
2. Every P2-W* packet (W1 through W6) has an identified compatibility strategy
   (Section 4.2) — **verified**
3. Phase 3-5 packets have compatibility notes (Sections 4.3-4.5 of the full
   design doc) — **verified**
4. Deprecation sequence is consistent with P0-W1 Section 12 — **verified**
   (cross-referenced element-by-element)
5. Test matrix covers dual-write scenarios (Section 5.4) — **verified** (7
   scenarios)
6. Test matrix covers dual-read scenarios (Section 5.5) — **verified** (8
   scenarios)
7. Test matrix covers migration scenarios (Section 5.6) — **verified** (7
   scenarios)
8. Cross-references to P0-W1, P0-W2, P0-W3 handoffs are consistent —
   **verified** (Section 10)

Verification commands run:
- `cargo fmt --all --check` — passed
- `cargo clippy --workspace --all-targets -- -D warnings` — passed
- `cargo test --workspace` — passed
