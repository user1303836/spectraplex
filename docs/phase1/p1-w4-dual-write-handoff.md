# P1-W4: Dual-Write Compatibility Layer — Handoff

**Phase:** Phase 1: Canonical Bronze And Control Plane
**Packet:** P1-W4
**Branch:** `p1-w4-dual-write-compat`
**Depends on:** P1-W1 (V2 core types), P1-W2 (V2 migrations), P1-W3 (V2 repo layer)

---

## Summary

This packet adds a dual-write layer so every V1 wallet ingestion also writes
canonical V2 records (`raw_transactions`, `target_matches`, `checkpoints`).
Wallet `IndexTarget` entries are auto-created via a compatibility shim. V2
writes are best-effort during early transition — failures are logged but never
abort the V1 write path. All existing API and CLI behavior stays identical.

## Files Changed

| File | Change |
|---|---|
| `adapters/src/dual_write.rs` | **New** — conversion functions, mapping, ensure_wallet_target, dual-write orchestration, unit tests |
| `adapters/src/lib.rs` | Added `pub mod dual_write` export |
| `cli/src/main.rs` | Wired dual-write: ensure_wallet_target before ingestion, dual-write methods after |
| `api/src/main.rs` | Wired dual-write: ensure_wallet_target before ingestion, dual-write methods after |
| `docs/phase1/p1-w4-dual-write-handoff.md` | **New** — this handoff document |

## Conversion Rules

### Chain → Network Mapping (Rollout Plan Section 2.4)

| V1 `Chain` | V2 `network` | V2 `source` |
|---|---|---|
| `Solana` | `solana-mainnet` | `rpc` |
| `Ethereum` | `ethereum-mainnet` | `rpc` |
| `Hyperliquid` | `hypercore-mainnet` | `rest` |

### Transaction Conversion (`v1_tx_to_v2_raw`)

| V1 field | V2 field | Notes |
|---|---|---|
| `id` | `id` | Direct copy |
| `timestamp` | `timestamp` | Direct copy |
| `tx_hash` | `tx_hash` | Direct copy |
| `raw_metadata` | `raw_metadata` | Direct copy |
| `chain` | `network` | Mapped via `chain_to_default_network` |
| — | `source` | Mapped via `chain_to_default_source` |
| — | `block_number` | Extracted from `raw_metadata` (Solana: `slot`, EVM: `block_number`, HL: None) |
| `user_id` | *(stripped)* | Replaced by `index_targets.owner_id` |
| `wallet_address` | *(stripped)* | Replaced by `target_matches` join |

### Checkpoint Conversion (`v1_checkpoint_to_v2`)

Builds a JSONB cursor per P0-W3 Section 5:

| Chain | Cursor fields |
|---|---|
| Solana | `{ "last_signature": ..., "last_slot": ... }` |
| Ethereum | `{ "last_block": ... }` |
| Hyperliquid | `{ "last_timestamp_ms": ... }` (seconds × 1000) |

### Target Match Construction

All matches use `match_reason = "sender"` since V1 wallet ingestion finds
transactions where the wallet is the sender/signer.

## Dual-Write Orchestration

1. **`ensure_wallet_target`**: idempotent lookup-or-create of an `IndexTarget`
   with `(kind=Wallet, network, address)`. Best-effort — failure is logged and
   the V1 path continues without V2 writes.

2. **`save_transactions_dual_write`**: V1 `save_transactions` first, then
   convert and save V2 `raw_transactions` + `target_matches`. V2 failure
   logged, not fatal.

3. **`save_checkpoint_dual_write`**: V1 `save_checkpoint` first, then convert
   and upsert V2 `checkpoint`. V2 failure logged, not fatal.

4. **`save_transactions_and_checkpoint_dual_write`**: V1 atomic
   `save_transactions_and_checkpoint` first, then V2 writes (best-effort).

## Test Coverage

Unit tests in `adapters/src/dual_write.rs`:

- Chain → network mapping (all 3 chains)
- Chain → source mapping (all 3 chains)
- V1 → V2 transaction conversion:
  - `user_id` and `wallet_address` stripped
  - Network mapped correctly for each chain
  - Block number extracted from metadata (Solana slot, EVM block_number)
  - Hyperliquid has no block_number
  - Direct fields preserved (id, hash, timestamp, metadata)
  - `ingestion_run_id` is None
- Checkpoint conversion:
  - Solana cursor: `{ last_signature, last_slot }`
  - Ethereum cursor: `{ last_block }`
  - Hyperliquid cursor: `{ last_timestamp_ms }` (seconds × 1000)
  - Missing optional fields handled
- Target match construction:
  - Correct count, unique IDs, correct linkage, "sender" reason
  - Empty input returns empty
- Batch conversion preserves order and IDs
- Full dual-write batch assembly

## Known Gaps

- **No database-level integration tests** for dual-write — requires a running
  PostgreSQL instance with V2 tables. The unit tests verify conversion logic
  and query construction.
- **`ensure_wallet_target` idempotency** is tested by logic review (lookup then
  create) but not by a database integration test.
- **V2 tables must exist** for dual-write to succeed. If the P1-W2 migration
  has not run, V2 writes will fail gracefully (logged, not fatal).
- **EVM network reclassification** caveat from Rollout Plan Section 2.4: all
  legacy Ethereum rows map to `ethereum-mainnet` regardless of actual chain.
  Reclassification is deferred to a future packet.

## Verification

- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — all tests pass, including new dual-write tests
- No V1 method signatures in `repo.rs` changed
- No API handler signatures or response shapes changed
- No CLI command flags or behavior changed
