# P2-W1: Connector Interface Redesign — Handoff

Phase: Phase 2 — Target-Centric Ingestion
Work Packet: P2-W1 — Connector interface redesign
Status: Complete (local)
Branch: `p2-w1-connector-interface`

## Summary

Introduced a filter-driven `Connector` trait that accepts typed `IndexTarget` specs instead of bare wallet addresses. Added typed filter spec structs, target validation, and a backward-compatible `LegacyConnectorAdapter` wrapper so existing CLI/API wallet flows continue to work through the new interface.

## What Changed

### New: `core/src/connector.rs`

New module defining the V2 connector abstraction:

- **`Connector` trait** — async trait with `backfill(target, cursor, limit)` and `stream(target, cursor)` methods. Takes `IndexTarget` as input instead of `wallet: &str`. The `stream` method has a default implementation that returns an error for backfill-only connectors.
- **`ConnectorCapabilities`** — declares which `TargetKind`s, `TargetMode`s, and `ChainFamily` a connector supports. Includes `can_service(target)` for routing decisions.
- **Typed filter specs** — eight filter structs matching `TARGET_MODEL.md`:
  - `WalletFilterSpec` (optional asset/direction narrowing)
  - `ContractFilterSpec` (EVM event signature and topic filters)
  - `ProgramFilterSpec` (Solana instruction discriminators, CPI inclusion)
  - `AccountFilterSpec` (access type narrowing)
  - `TopicFilterSpec` (EVM topic-based log filters, required)
  - `MarketFilterSpec` (Hyperliquid data type narrowing)
  - `PoolFilterSpec` (pool metadata and event types)
  - `ProtocolFilterSpec` (composite multi-address targets, required)
- **`extract_filter_spec(target)`** — parses `IndexTarget.filter_spec` JSONB into the typed enum `TypedFilterSpec`. Returns defaults for optional specs, errors for required ones.
- **`validate_target(target)`** — validates kind-family compatibility, address presence, and filter_spec presence. Returns a list of validation errors.

### New: `adapters/src/compat.rs`

Compatibility layer bridging V1 `ChainIngestor` to V2 `Connector`:

- **`legacy_wallet_target(chain_str, wallet, owner_id)`** — synthesizes an `IndexTarget` from legacy CLI/API parameters. Handles address normalization per chain family.
- **`legacy_wallet_target_from_chain(chain, wallet, owner_id)`** — same but accepts a `Chain` enum.
- **`v1_checkpoint_to_cursor(checkpoint)`** — converts V1 `IndexerCheckpoint` to V2 opaque cursor JSON.
- **`cursor_to_v1_checkpoint(chain, wallet, cursor)`** — reverse conversion.
- **`LegacyConnectorAdapter<I: ChainIngestor>`** — wraps any V1 adapter (Solana RPC, Solana gRPC, EVM, Hyperliquid) and implements `Connector`. Only wallet targets are supported. Converts V1 `Transaction` results to V2 `RawTransaction`s (stripping `user_id` and `wallet_address`).

### Modified: `core/src/lib.rs`

Added `pub mod connector;`.

### Modified: `core/Cargo.toml`

Added `tokio = { version = "1", features = ["sync"] }` dependency (needed for `mpsc::Receiver` in the `Connector::stream` return type).

### Modified: `adapters/src/lib.rs`

Added `pub mod compat;`.

## Design Decisions

1. **Trait shape**: `Connector::backfill` takes an opaque `serde_json::Value` cursor instead of a typed checkpoint. This keeps the trait generic across chain families that have fundamentally different checkpoint shapes (slot for Solana, block for EVM, timestamp for Hyperliquid).

2. **`stream` default**: The default `stream` implementation returns an error. This means connectors that only support backfill (like `SolanaRpcConnector`, `EvmConnector`, `HyperliquidRestConnector`) only need to implement `backfill`.

3. **Compatibility via wrapping, not rewriting**: The `LegacyConnectorAdapter` wraps existing `ChainIngestor` implementations rather than modifying them. This means P2-W3/W4/W5 can independently rewrite each chain adapter to implement `Connector` directly, without blocking on this packet.

4. **Address normalization in target construction**: `legacy_wallet_target` normalizes addresses (EVM lowercase, Solana passthrough) per `TARGET_MODEL.md` Section 6.4, preventing duplicate targets from case differences.

5. **No changes to existing adapters, CLI, or API**: This packet is purely additive. The V1 `ChainIngestor` trait and all existing code paths are untouched. P2-W2 will wire the CLI/API to optionally use the new interface.

## Test Coverage

### `core/src/connector.rs` — 36 tests

- `ConnectorCapabilities`: target kind support, mode support (including `Both` semantics), `can_service` cross-family check
- Filter spec extraction for all 8 target kinds: defaults, with values, required-field enforcement
- Filter spec serde roundtrip for all complex types
- Target validation: valid targets, invalid kind-family combos, missing address, empty address, missing filter_spec, multiple errors, all target kinds

### `adapters/src/compat.rs` — 17 tests

- `legacy_wallet_target`: Solana, Ethereum (with address normalization), Hyperliquid, unknown chain
- `legacy_wallet_target_from_chain`: Solana, Ethereum
- Checkpoint cursor conversion: roundtrip, non-compat cursor, unknown chain
- `v1_tx_to_raw`: Solana slot, EVM block_number, no block_number, user_id/wallet_address stripping
- `LegacyConnectorAdapter`: capabilities check, wallet backfill, non-wallet rejection, cursor passthrough, missing address

### Totals

- 53 new tests (36 in core, 17 in adapters)
- All 371 workspace tests pass (128 adapters, 75 core, 117 api, 18 cli, 22 migration, 11 parser)

## Verification

```
cargo fmt --all --check    # pass
cargo clippy --workspace --all-targets -- -D warnings  # pass
cargo test --workspace     # 371 passed, 0 failed
```

## Downstream Dependencies

This packet unlocks:

- **P2-W2**: CLI and API target registration flow (can now construct `IndexTarget` from new endpoints and route through `Connector`)
- **P2-W3**: EVM connector rewrite (implement `Connector` directly instead of `ChainIngestor`, fix wallet vs contract semantics)
- **P2-W4**: Solana gRPC connector rewrite (implement `Connector` directly, fix wallet vs program target semantics)
- **P2-W5**: Hyperliquid target model (implement `Connector` directly for user and market targets)

## Files Changed

| File | Change |
|------|--------|
| `core/src/connector.rs` | New — V2 Connector trait, typed filter specs, validation |
| `core/src/lib.rs` | Modified — added `pub mod connector` |
| `core/Cargo.toml` | Modified — added `tokio` dependency |
| `adapters/src/compat.rs` | New — legacy compatibility wrapper |
| `adapters/src/lib.rs` | Modified — added `pub mod compat` |
| `docs/phase2/p2-w1-connector-interface-handoff.md` | New — this document |
