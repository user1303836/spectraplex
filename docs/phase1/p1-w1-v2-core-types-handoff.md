# P1-W1: V2 Core Types — Handoff

Status: Complete
Date: 2026-03-08
Phase: Phase 1 — Canonical Bronze And Control Plane
Branch: `codex/p1-w1-v2-core-types`

## Summary

Introduces V2 domain types in `core/src/v2.rs` as a new module. All existing V1 types in `core/src/models.rs` remain completely untouched. No changes to migrations, adapters, API, or CLI.

## Type Inventory

| Type | Kind | Spec Reference |
|---|---|---|
| `ChainFamily` | enum (3 variants) | P0-W3 Section 2 |
| `FinalityModel` | enum (4 variants) | P0-W3 Section 2 / Section 4 |
| `TargetKind` | enum (8 variants) | P0-W2 Section 2 |
| `TargetMode` | enum (3 variants) | P0-W2 Section 4 |
| `Network` | struct | P0-W3 Section 1.1 / Section 3.1 |
| `IndexTarget` | struct | P0-W1 Section 3.2 |
| `RawTransaction` | struct | P0-W1 Section 3.1 |
| `TargetMatch` | struct | P0-W1 Section 3.2 |
| `IngestionRun` | struct | P0-W1 Section 3.4 |
| `Checkpoint` | struct | P0-W1 Section 3.4, P0-W3 Section 5 |
| `DatasetVersion` | struct | P0-W1 Section 3.5 |
| `IngestionBatch` | struct | P0-W1 Section 4.2 |

## Helpers

| Function / Method | Description | Spec Reference |
|---|---|---|
| `normalize_evm_address` | Lowercases hex addresses | P0-W2 Section 6.4 |
| `normalize_solana_address` | Base58 passthrough | P0-W2 Section 6.4 |
| `From<Chain> for ChainFamily` | Legacy interop conversion | P0-W3 Section 3.5 |
| `TargetKind::valid_for_chain_family` | Validity matrix check | P0-W2 Section 3 |

## Enum Serialization

| Enum | Serde format | Example |
|---|---|---|
| `ChainFamily` | lowercase | `"solana"`, `"evm"`, `"hyperliquid"` |
| `FinalityModel` | kebab-case | `"probabilistic-slot"`, `"instant"` |
| `TargetKind` | snake_case | `"wallet"`, `"topic_filter"` |
| `TargetMode` | snake_case | `"backfill"`, `"stream"`, `"both"` |

All enums also implement `Display` and `FromStr` via strum with matching serialization formats.

## Key Design Decisions

1. **RawTransaction has no `user_id` or `wallet_address`** — per P0-W1 Section 3.1 rules 1-2. Consumer identity and wallet association happen through `IndexTarget.owner_id` and `TargetMatch` respectively.

2. **`IndexTarget.filter_spec` is `Option<serde_json::Value>`** — JSONB flexibility for chain-specific filtering without requiring schema migration for new filter fields.

3. **`TargetKind::Account` returns `valid=true` for EVM** — the P0-W2 Section 3 matrix marks it as "Limited" but still usable. Callers wanting to warn about limited support should implement that logic separately.

4. **`ChainFamily` serializes to lowercase** to match the `chain_family_enum` SQL values (`solana`, `evm`, `hyperliquid`).

5. **`FinalityModel` serializes to kebab-case** to match the P0-W3 SQL column values (`probabilistic-slot`, etc.).

## Verification

- `cargo fmt --all --check` — pass
- `cargo clippy --workspace --all-targets -- -D warnings` — pass
- `cargo test --workspace` — pass (all existing tests + 26 new V2 unit tests)

## Files Changed

| File | Change |
|---|---|
| `core/Cargo.toml` | Added `strum` dependency |
| `core/src/lib.rs` | Added `pub mod v2` export |
| `core/src/v2.rs` | New module with all V2 domain types |
| `docs/phase1/p1-w1-v2-core-types-handoff.md` | This document |

## What This Unlocks

- **P1-W2** (migrations) can reference these types for table definitions
- **P1-W3** (repo layer) can use these types for repository method signatures
- **P2-W1** (connector redesign) can use `IndexTarget`, `Checkpoint`, and `IngestionBatch` for the new `Connector` trait
