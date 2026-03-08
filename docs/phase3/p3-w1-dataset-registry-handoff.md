# P3-W1: Dataset Registry and Versioning — Handoff

## Deliverables

### New modules and files

- **`core/src/materializer.rs`** — Dataset registry types and Materializer trait:
  - `DatasetName` enum with 7 Silver datasets (LedgerEntries, TokenTransfers, NativeBalanceDeltas, DecodedEvents, HlFills, HlFunding, Positions)
  - `DatasetDescriptor` struct for dataset metadata and lineage
  - `Materializer` trait for parser version tracking
  - `RegenerationScope` and `RegenerationRequest` types for Bronze-to-Silver regeneration

- **`core/src/v2.rs`** — Extended with:
  - `DatasetVersionStatus` enum (Active, Superseded, Failed)
  - `status` field on `DatasetVersion` struct with backward-compatible default

- **`migrations/20260309000000_add_dataset_versioning.sql`** — Schema changes:
  - `status TEXT DEFAULT 'active'` column on `dataset_versions`
  - `UNIQUE (dataset_name, version)` constraint on `dataset_versions`
  - `dataset_version_id UUID REFERENCES dataset_versions(id)` nullable column on `ledger_entries`
  - Index on `ledger_entries(dataset_version_id)`

### Extended modules

- **`adapters/src/v2_repo.rs`** — New repository methods:
  - `list_datasets()` — distinct dataset names
  - `list_dataset_versions(dataset_name)` — all versions ordered by version desc
  - `get_dataset_version_by_id(id)` — single version lookup
  - `mark_version_superseded(id)` — status transition
  - `count_records_by_version(dataset_version_id)` — ledger_entries count

- **`adapters/src/solana_parser.rs`** — `SolanaLedgerMaterializer` implementing Materializer
- **`adapters/src/evm_parser.rs`** — `EvmLedgerMaterializer` implementing Materializer
- **`adapters/src/hyperliquid_parser.rs`** — `HyperliquidLedgerMaterializer` implementing Materializer

## Decisions

1. **DatasetName as a Rust enum** — Dataset names are an enum rather than free-form strings. This ensures compile-time coverage checks and prevents typos. The `as_sql_str()` method provides stable SQL-compatible values.

2. **Status as TEXT, not an SQL enum** — The `status` column on `dataset_versions` uses `TEXT DEFAULT 'active'` rather than a custom SQL enum type. This simplifies migration and avoids the PostgreSQL enum extension pain. Validation is enforced at the application layer.

3. **Nullable `dataset_version_id` on `ledger_entries`** — During the transition period, existing ledger entries will have `NULL` for this field. New materializations produced with P3-W2+ will populate it. This is a deliberate backward-compatible choice.

4. **Materializer trait is non-async** — The trait methods are synchronous because they return static metadata. The actual materialization logic (parsing) lives in the existing `parse_*_transaction` functions, not in the trait itself. The trait is about identity and versioning, not execution.

5. **Parser hash values are human-readable stable strings** — Each chain materializer has a distinct `parser_hash` value. These are placeholder hashes for v1 parsers and should be updated to actual content hashes when parser logic changes.

6. **All three materializers produce `ledger_entries`** — This is the only Silver dataset with parsers today. P3-W2/W3/W4 will add materializers for the other 6 dataset types.

## Open Questions for P3-W2/W3/W4

1. **Token transfer extraction** — Should `token_transfers` be populated by the same parse pass that creates `ledger_entries`, or by a separate materializer reading Bronze? A separate materializer is cleaner but may duplicate Bronze reads.

2. **Native balance deltas** — For Solana, pre/post balances are in the transaction metadata. For EVM, balance changes require trace calls or block-level diffs. The EVM materializer for `native_balance_deltas` may need a different Bronze source.

3. **Decoded events** — `decoded_events` requires ABI registries (EVM) or IDL registries (Solana). Should the materializer embed known ABIs or accept them as configuration?

4. **HL fills and funding** — The existing `hl_fills` extension table may need to be migrated or dual-written alongside the new Silver `hl_fills` dataset. The mapping should preserve existing data.

5. **Positions** — `positions` is a derived dataset that spans fills, funding, and liquidations. It may need to read from other Silver datasets rather than directly from Bronze. This is architecturally different from the other materializers.

6. **Version lifecycle automation** — `mark_version_superseded` is manual today. P3-W5 or later should add automatic supersession when a new version is created for the same dataset.

7. **Regeneration execution** — `RegenerationRequest` and `RegenerationScope` define the _what_ but not the _how_. The actual regeneration orchestrator (reading Bronze, running materializers, writing Silver) will be built in a later work packet.
