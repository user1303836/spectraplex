# P2-W6: End-to-End Ingestion Compatibility Suite — Handoff

## Summary

Added comprehensive integration test coverage verifying the full V2 ingestion
pipeline for all supported target classes. The test file
`adapters/tests/ingestion_compat_test.rs` contains 26 tests covering adapter
capabilities, DB round-trips, multi-target linkage, checkpoint semantics,
lifecycle transitions, error paths, and cross-adapter consistency.

## What Changed

**New file: `adapters/tests/ingestion_compat_test.rs`** (26 tests)

### Test Categories

| Category | Tests | Description |
|---|---|---|
| Capability declarations | 8 | Verify each Connector impl returns correct `supported_target_kinds`, `supported_modes`, `chain_family` |
| Cross-adapter coverage | 1 | Verify all 8 TargetKind variants are either covered by an adapter or are documented future gaps |
| DB round-trips | 6 | Full pipeline for wallet (Solana), EVM contract, Solana program, HL wallet, HL market, EVM topic_filter |
| Multi-target linkage | 1 | One raw_transaction linked to 3 targets with distinct match_reasons |
| Checkpoint upsert | 1 | Idempotency: second upsert overwrites cursor, produces exactly one row |
| Ingestion run lifecycle | 1 | Create running -> update to completed with records_written and finished_at |
| Error paths (unsupported kind) | 5 | Each adapter rejects unsupported TargetKind with clear error message |
| Error paths (wrong family) | 3 | SolanaAdapter, EvmAdapter, HyperliquidAdapter reject wrong chain families |
| Validation consistency | 1 | validate_target and can_service agree for all (TargetKind, ChainFamily) pairs |

### DB Round-Trip Coverage

Each round-trip test exercises the full pipeline:
1. Register IndexTarget via `create_index_target`
2. Create IngestionRun (where applicable)
3. Save synthetic RawTransactions via `save_raw_transactions`
4. Create TargetMatches via `save_target_matches`
5. Upsert Checkpoint via `upsert_checkpoint_v2`
6. Verify all data retrieval: `get_index_target`, `get_matches_by_target`,
   `get_checkpoint_v2`, `get_raw_transactions_by_run`

### Target Classes Covered

| Target Class | Chain Family | Network | Round-Trip | Error Path |
|---|---|---|---|---|
| Wallet | Solana | solana-mainnet | Yes | Yes |
| Contract | EVM | ethereum-mainnet | Yes | Yes |
| Program | Solana | solana-mainnet | Yes | Yes |
| Account | Solana | solana-mainnet | Via multi-target test | N/A (gRPC supports it) |
| TopicFilter | EVM | ethereum-mainnet | Yes | N/A (EVM adapter supports it) |
| Market | Hyperliquid | hypercore-mainnet | Yes | Yes |
| Wallet (HL) | Hyperliquid | hypercore-mainnet | Yes | Yes |
| Pool | EVM/Solana | — | Covered by consistency test (future gap) | N/A |
| Protocol | EVM/Solana | — | Covered by consistency test (future gap) | N/A |

### Adapter Coverage Matrix

| Adapter | Wallet | Contract | Program | Account | TopicFilter | Market |
|---|---|---|---|---|---|---|
| SolanaAdapter | Yes | — | — | — | — | — |
| SolanaGrpcAdapter | Yes | — | Yes | Yes | — | — |
| EvmAdapter | Yes | Yes | — | — | Yes | — |
| HyperliquidAdapter | Yes | — | — | — | — | Yes |
| LegacyConnectorAdapter | Yes | — | — | — | — | — |

### Known Future Gaps

The cross-adapter coverage test documents these as known gaps (valid for their
chain families but no adapter implementation yet):

- `Pool` on Solana and EVM
- `Protocol` on Solana and EVM
- `Account` on EVM

## Verification

All three CI gates pass:

```
cargo fmt --all --check        # OK
cargo clippy --workspace --all-targets -- -D warnings  # OK
cargo test --workspace         # OK (26 new + all existing tests pass)
```

## Dependencies

- Requires PostgreSQL for DB round-trip tests (ephemeral databases)
- CI already includes PostgreSQL 16 service container
- No new crate dependencies required

## Follow-Ups

- Pool and Protocol adapters are future work (Phase 3+)
- Account support on EVM is a known gap documented in the consistency test
- When new adapters are added, they should be included in the capability and
  coverage tests
