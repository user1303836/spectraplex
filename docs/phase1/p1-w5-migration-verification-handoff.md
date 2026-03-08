# P1-W5: Migration and Rollback Verification — Handoff

**Phase:** Phase 1: Canonical Bronze And Control Plane
**Packet:** P1-W5
**Branch:** `p1-w5-migration-verification`
**Depends on:** P1-W2 (V2 migrations), P1-W3 (V2 repo layer)

---

## Summary

This packet adds a dedicated migration integration test suite that verifies the
V2 schema is correct across fresh-DB and upgraded-DB paths. Tests run against a
real PostgreSQL instance (no mocks), creating and dropping ephemeral databases
per test for full isolation. The suite covers all 10 migrations (7 V1 + 3 V2),
all 5 uniqueness constraints, 7 foreign keys, multi-target linkage, network seed
data, enum correctness, and V1-V2 coexistence. Tests include a `require_pg!`
skip guard so they gracefully skip when PostgreSQL is unavailable, and the CI
workflow provisions a PostgreSQL 16 service container to run them.

## Files Changed

| File | Change |
|---|---|
| `adapters/tests/migration_test.rs` | **New** — 22 integration tests against real PostgreSQL with graceful skip guard |
| `adapters/Cargo.toml` | Added `[dev-dependencies]` section with `log = "0.4"` |
| `.github/workflows/ci.yml` | Added PostgreSQL 16 service container and `TEST_DATABASE_URL` env to CI test job |
| `docs/phase1/p1-w5-migration-verification-handoff.md` | **New** — this handoff document |

## Test Coverage (22 tests)

### Fresh-DB Path
| Test | What it verifies |
|---|---|
| `fresh_db_all_tables_exist` | All 13 tables (7 V1 + 6 V2 + networks) exist after applying all 10 migrations on an empty database |
| `fresh_db_v2_table_columns` | Every column on every V2 table matches the migration DDL |

### Upgraded-DB Path
| Test | What it verifies |
|---|---|
| `upgraded_db_v1_data_survives_v2_migration` | Apply 7 V1 migrations, insert V1 data (transaction, ledger entry, checkpoint), apply 3 V2 migrations, verify V1 data is intact and V2 tables are available |

### Uniqueness Constraints
| Test | Constraint verified |
|---|---|
| `uniqueness_index_targets_address` | `uq_index_targets_address` on `(kind, network, address)` — rejects duplicates, allows different kind for same network+address |
| `uniqueness_index_targets_filter_spec` | `uq_index_targets_filter_spec` on `(kind, network, filter_spec_hash) WHERE filter_spec_hash IS NOT NULL` — rejects duplicates, verifies NULL exemption, allows different kind for same network+filter_spec_hash |
| `uniqueness_raw_transactions_network_tx_hash` | `uq_raw_transactions_network_tx_hash` on `(network, tx_hash)` — rejects duplicates, allows same tx_hash on different network |
| `uniqueness_target_matches_target_raw_tx` | `uq_target_matches_target_raw_tx` on `(target_id, raw_transaction_id)` — rejects duplicates |
| `uniqueness_checkpoints_target_network_source` | `uq_checkpoints_target_network_source` on `(target_id, network, source)` — rejects duplicates, allows different source for same target+network |

### Foreign Keys
| Test | FK verified |
|---|---|
| `fk_raw_transactions_network_references_networks` | `raw_transactions.network → networks.id` rejects bogus network |
| `fk_target_matches_target_id_references_index_targets` | `target_matches.target_id → index_targets.id` rejects orphan |
| `fk_target_matches_raw_transaction_id_references_raw_transactions` | `target_matches.raw_transaction_id → raw_transactions.id` rejects orphan |
| `fk_checkpoints_target_id_references_index_targets` | `checkpoints.target_id → index_targets.id` rejects orphan |
| `fk_ingestion_runs_target_id_references_index_targets` | `ingestion_runs.target_id → index_targets.id` rejects orphan |
| `fk_raw_transactions_ingestion_run_id_references_ingestion_runs` | `raw_transactions.ingestion_run_id → ingestion_runs.id` rejects orphan |
| `fk_index_targets_network_references_networks` | `index_targets.network → networks.id` rejects bogus network |

### Multi-Target Linkage
| Test | What it verifies |
|---|---|
| `multi_target_linkage_single_raw_tx` | Creates 1 raw_transaction and 2 index_targets, links both via target_matches, verifies both matches exist and can be queried by either target |

### Network Seed Data
| Test | What it verifies |
|---|---|
| `network_seed_data_13_networks` | All 13 canonical networks exist with correct chain_family, is_testnet, finality_model, and block_time_ms values |

### Enum Types
| Test | What it verifies |
|---|---|
| `v2_enum_chain_family_values` | `chain_family_enum` has exactly: solana, evm, hyperliquid |
| `v2_enum_target_kind_values` | `target_kind_enum` has exactly: wallet, contract, program, account, topic_filter, market, pool, protocol |
| `v2_enum_target_mode_values` | `target_mode_enum` has exactly: backfill, stream, both |

### V1-V2 Coexistence
| Test | What it verifies |
|---|---|
| `v1_v2_coexistence` | Both V1 `transactions` and V2 `raw_transactions` + `target_matches` can be populated and queried independently in the same database |
| `v1_enums_preserved` | V1 enums (`chain_enum`, `entry_type_enum`) are untouched by V2 migrations |

## Test Infrastructure

- Tests use **real PostgreSQL** (not mocked). Requires a local PostgreSQL instance.
- Each test starts with `require_pg!()` — a macro that gracefully skips the test if PostgreSQL is unreachable (2-second timeout), preventing panics in environments without a database.
- CI provisions a **PostgreSQL 16 service container** and sets `TEST_DATABASE_URL` so all tests run automatically in GHA.
- Each test creates an **ephemeral database** (`spx_test_{prefix}_{uuid}`) and drops it on cleanup.
- Connection URL defaults to `postgres://localhost/postgres`; override with `TEST_DATABASE_URL`.
- `run_all_migrations` uses `sqlx::migrate!("../migrations").run()` — identical to production.
- `run_n_migrations` manually applies the first N migrations with tracking via `_sqlx_migrations` to support the upgraded-DB test path.
- Tests run in parallel (4 threads) for speed; each test is fully isolated.

## Verification Results

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ✅ Clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ Clean |
| `cargo test -p spectraplex-core -p spectraplex-adapters -p spectraplex-cli` | ✅ 199 tests pass (110 adapters unit + 22 migration + 11 solana parser + 18 CLI + 38 core) |

### Pre-existing issue

`spectraplex-api::tests::test_semaphore_limits_concurrent_jobs` is a pre-existing flaky test (race condition in concurrent job semaphore test). It passes in isolation but intermittently fails when run alongside all other API tests. This is unrelated to P1-W5 changes.

## Dependencies Satisfied

- **P1-W2** (migrations): All 10 migration files validated by the test suite
- **P1-W3** (V2 repo): V2 tables confirmed to accept the data shapes used by the repo layer

## Follow-ups

- The flaky `test_semaphore_limits_concurrent_jobs` in the API crate predates this work and should be addressed separately
