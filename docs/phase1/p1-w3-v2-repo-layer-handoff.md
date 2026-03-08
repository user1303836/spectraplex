# P1-W3: V2 Repository Layer — Handoff

## Summary

Added `adapters/src/v2_repo.rs` implementing repository methods for all V2 tables introduced in P1-W2. Methods are added as `impl Repository` so callers use the same `Repository` value used for V1 wallet-scoped queries. No existing V1 method signatures were changed.

## Method Inventory

### Networks

| Method | Signature | Notes |
|--------|-----------|-------|
| `get_network` | `(&self, id: &str) -> Result<Option<Network>>` | Lookup by primary key |
| `list_networks` | `(&self) -> Result<Vec<Network>>` | All networks, ordered by id |
| `list_networks_by_family` | `(&self, family: ChainFamily) -> Result<Vec<Network>>` | Filtered by `chain_family_enum` cast |

### IndexTargets

| Method | Signature | Notes |
|--------|-----------|-------|
| `create_index_target` | `(&self, target: &IndexTarget) -> Result<IndexTarget>` | Single-row INSERT with enum casts |
| `get_index_target` | `(&self, id: Uuid) -> Result<Option<IndexTarget>>` | Lookup by UUID |
| `get_index_target_by_address` | `(&self, kind, network, address) -> Result<Option<IndexTarget>>` | Uses `target_kind_enum` cast |
| `list_index_targets_by_network` | `(&self, network: &str) -> Result<Vec<IndexTarget>>` | Ordered by `created_at` |
| `list_index_targets_by_kind` | `(&self, kind: TargetKind) -> Result<Vec<IndexTarget>>` | Uses `target_kind_enum` cast |

### RawTransactions

| Method | Signature | Notes |
|--------|-----------|-------|
| `save_raw_transactions` | `(&self, txs: &[RawTransaction]) -> Result<()>` | Batch insert, chunked at 500, `ON CONFLICT (network, tx_hash) DO NOTHING` |
| `get_raw_transaction_by_hash` | `(&self, network, tx_hash) -> Result<Option<RawTransaction>>` | Lookup by unique key |
| `get_raw_transactions_by_run` | `(&self, run_id: Uuid) -> Result<Vec<RawTransaction>>` | Ordered by timestamp |

### TargetMatches

| Method | Signature | Notes |
|--------|-----------|-------|
| `save_target_matches` | `(&self, matches: &[TargetMatch]) -> Result<()>` | Batch insert, chunked at 500, `ON CONFLICT (target_id, raw_transaction_id) DO NOTHING` |
| `get_matches_by_target` | `(&self, target_id, limit, offset) -> Result<Vec<(TargetMatch, RawTransaction)>>` | JOIN query, ordered by `rt.timestamp DESC` |
| `get_matches_by_raw_tx` | `(&self, raw_tx_id: Uuid) -> Result<Vec<TargetMatch>>` | All targets matching a raw tx |

### IngestionRuns

| Method | Signature | Notes |
|--------|-----------|-------|
| `create_ingestion_run` | `(&self, run: &IngestionRun) -> Result<()>` | Single-row INSERT |
| `update_ingestion_run_status` | `(&self, id, status, finished_at, records_written, error_message) -> Result<()>` | Partial UPDATE |
| `get_ingestion_run` | `(&self, id: Uuid) -> Result<Option<IngestionRun>>` | Lookup by UUID |
| `list_ingestion_runs_by_target` | `(&self, target_id: Uuid) -> Result<Vec<IngestionRun>>` | Ordered by `started_at DESC` |

### Checkpoints (V2)

| Method | Signature | Notes |
|--------|-----------|-------|
| `upsert_checkpoint_v2` | `(&self, cp: &Checkpoint) -> Result<()>` | `ON CONFLICT (target_id, network, source) DO UPDATE` cursor and updated_at |
| `get_checkpoint_v2` | `(&self, target_id, network, source) -> Result<Option<Checkpoint>>` | Lookup by unique key |

### DatasetVersions

| Method | Signature | Notes |
|--------|-----------|-------|
| `create_dataset_version` | `(&self, dv: &DatasetVersion) -> Result<()>` | Single-row INSERT |
| `get_latest_dataset_version` | `(&self, dataset_name: &str) -> Result<Option<DatasetVersion>>` | `ORDER BY version DESC LIMIT 1` |

## Public Query Builders

Exposed as `pub fn` for unit testing:

- `build_raw_transaction_insert` — batch VALUES with 9 params per row
- `build_target_match_insert` — batch VALUES with 5 params per row
- `build_index_target_insert` — single-row with `$2::target_kind_enum`, `$4::chain_family_enum`, `$7::target_mode_enum`
- `build_checkpoint_upsert` — single-row with ON CONFLICT DO UPDATE
- `build_ingestion_run_insert` — single-row with 11 params
- `build_dataset_version_insert` — single-row with 6 params

## Enum Helper Functions

Six public conversion functions with full roundtrip coverage:

- `chain_family_to_sql` / `sql_to_chain_family`
- `target_kind_to_sql` / `sql_to_target_kind`
- `target_mode_to_sql` / `sql_to_target_mode`

## Design Decisions

1. **`impl Repository` in a separate file**: Keeps V2 methods cleanly separated from V1 while sharing the same `Repository` struct and connection pool. No new struct needed.

2. **`pub(crate) fn pool()`**: Added a crate-visible accessor on `Repository` so `v2_repo.rs` can reach the pool without making the field public. This is the only change to `repo.rs`.

3. **No enum casts on raw_transactions or ingestion_runs**: These tables use plain `TEXT` columns for `network`, `source`, `mode`, `status` — matching the migration DDL. Enum casts are only used where the migration defines actual PostgreSQL enum types (`chain_family_enum`, `target_kind_enum`, `target_mode_enum`).

4. **Query builders are public free functions**: Makes them unit-testable without a database connection. The async methods on `Repository` call these builders then execute against the pool.

5. **Batch size matches V1**: `V2_BATCH_SIZE = 500`, same as V1's `BATCH_SIZE`, for consistent chunking behavior.

6. **`get_matches_by_target` returns joined tuples**: Returns `Vec<(TargetMatch, RawTransaction)>` to avoid N+1 queries. The JOIN aliases the raw_transaction id column as `rt_id` to avoid column name collision.

## Verification Results

```
cargo fmt --all --check     — pass
cargo clippy --workspace --all-targets -- -D warnings  — pass
cargo test --workspace      — 262 tests pass (18 new V2 repo tests, 0 regressions)
```

### New Test Coverage (18 tests)

- `chain_family_sql_roundtrip` — all 3 variants
- `chain_family_sql_unknown` — error case
- `target_kind_sql_roundtrip` — all 8 variants
- `target_kind_sql_unknown` — error case
- `target_mode_sql_roundtrip` — all 3 variants
- `target_mode_sql_unknown` — error case
- `raw_tx_insert_single` — query shape and ON CONFLICT clause
- `raw_tx_insert_multiple` — multi-row parameter numbering
- `raw_tx_insert_param_count` — 5 rows × 9 params = $45
- `target_match_insert_single` — query shape and ON CONFLICT clause
- `target_match_insert_multiple` — multi-row parameter numbering
- `index_target_insert_uses_enum_casts` — verifies `::target_kind_enum`, `::chain_family_enum`, `::target_mode_enum`
- `index_target_insert_has_11_params` — parameter count
- `checkpoint_upsert_on_conflict` — ON CONFLICT DO UPDATE clause
- `checkpoint_upsert_has_6_params` — parameter count
- `ingestion_run_insert_has_11_params` — parameter count
- `dataset_version_insert_has_6_params` — parameter count
- `v2_batch_size_matches_v1` — constant value

## Files Changed

- `adapters/src/v2_repo.rs` — new (V2 repository methods + 18 tests)
- `adapters/src/repo.rs` — added `pub(crate) fn pool()` accessor (4 lines)
- `adapters/src/lib.rs` — added `pub mod v2_repo`

## Follow-ups for P1-W4

- Wire dual-write from existing wallet ingestion paths through `save_raw_transactions` + `save_target_matches`
- Add transactional wrappers that combine raw insert + target match + checkpoint in a single DB transaction
- Integration tests against a real PostgreSQL instance to verify enum casts, FK constraints, and upsert behavior
