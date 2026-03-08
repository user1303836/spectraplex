# P2-W3: Fix EVM Ingestion Semantics — Handoff

## Summary

This work packet separates wallet activity indexing from contract event indexing
in the EVM adapter. It fixes the incorrect `eth_getLogs` address filter for
wallet targets and implements the V2 `Connector` trait for `EvmAdapter` with
correct semantics for wallet, contract, and topic_filter target kinds.

## Problem

The original `EvmAdapter.fetch_history()` used the **wallet address** as the
`eth_getLogs` address filter. This returns logs *emitted by* that address (i.e.,
treating it as a contract), not logs *involving* the wallet as a participant.
For EOA wallets this returns nothing useful; for contract wallets it returns the
wrong data (contract events, not wallet activity).

## Solution

### Three distinct ingestion paths

1. **Wallet target** — topic-filtered ERC-20 Transfer logs.

   Two `eth_getLogs` calls per block chunk:
   - Transfer events with wallet in `topic[1]` (outgoing / sender)
   - Transfer events with wallet in `topic[2]` (incoming / receiver)

   Results are deduplicated by `(tx_hash, log_index)` to handle self-transfers.
   Transaction-level fields (value, from, to, gas_used, effective_gas_price) are
   still fetched per unique tx hash and attached to the first log, preserving
   native ETH value detection and gas fee enrichment for the downstream parser.

2. **Contract target** — address-filtered logs (the standard `eth_getLogs`
   address filter).

   Returns all events emitted by the specified contract. This is the correct
   semantic for indexing contract activity.

3. **Topic filter target** — arbitrary topic-position filters with optional
   address narrowing.

   Parses the `TopicFilterSpec` from the target's `filter_spec` JSONB and builds
   the appropriate `eth_getLogs` filter with up to 4 topic positions.

### New adapter methods

| Method | Purpose |
|--------|---------|
| `fetch_logs_by_topics()` | Generalized log fetching with optional address filter and up to 4 topic positions |
| `fetch_wallet_erc20_logs()` | Two topic-filtered calls for wallet Transfer events, deduplicated |
| `enrich_tx_data()` | Shared tx/receipt enrichment (extracted from legacy path) |
| `logs_to_raw_transactions()` | Convert logs + enrichment to V2 `RawTransaction` records |
| `wallet_backfill()` | Connector dispatch: wallet path |
| `contract_backfill()` | Connector dispatch: contract path |
| `topic_filter_backfill()` | Connector dispatch: topic filter path |

### Connector trait implementation

```rust
impl Connector for EvmAdapter {
    fn capabilities() -> ConnectorCapabilities {
        // supported_target_kinds: [Wallet, Contract, TopicFilter]
        // supported_modes: [Backfill]
        // chain_family: Evm
    }

    async fn backfill(target, cursor, limit) -> IngestionBatch {
        match target.kind {
            Wallet     => wallet_backfill(...)
            Contract   => contract_backfill(...)
            TopicFilter => topic_filter_backfill(...)
            _          => error (unsupported)
        }
    }
}
```

### Output format

All three paths emit `RawTransaction` records — target-agnostic, with no
`wallet_address` or `user_id` fields. Each record includes:

- `network`: e.g. "ethereum-mainnet"
- `tx_hash`: full hex tx hash
- `timestamp`: block timestamp as i64
- `block_number`: from the log's block
- `raw_metadata`: log data + enrichment fields
- `source`: descriptive string identifying the ingestion path
  - `"evm-rpc-wallet-backfill"`
  - `"evm-rpc-contract-backfill"`
  - `"evm-rpc-topic-filter-backfill"`

### Backward compatibility

The legacy `ChainIngestor` implementation is preserved unchanged. The
`LegacyConnectorAdapter` in `compat.rs` continues to wrap it for V1 callers.
Existing CLI/API paths that use `ChainIngestor::fetch_history()` are unaffected.

## Limitations and known gaps

1. **Native-only ETH transfers** (transfers with no associated log events) are
   not discoverable via `eth_getLogs` topic filtering. They require block-level
   transaction scanning or trace APIs (`trace_block`, `debug_traceBlock`). The
   current implementation preserves the existing behavior: tx-level value/from/to
   fields are enriched onto the first log per transaction, which the parser uses
   for native value detection. Transactions with *only* native value and *no*
   log events will be missed by the wallet backfill path.

2. **Streaming not yet implemented.** The `Connector::stream()` default returns
   an error. EVM streaming via `eth_subscribe` or polling will be a separate
   work packet.

3. **Cursor/checkpoint persistence** is not yet wired. The backfill methods
   compute a new cursor internally but the checkpoint is not yet persisted to
   the V2 checkpoint table. This will be handled by the orchestrator layer.

4. **Topic filter arrays** (OR-matching multiple values in a single topic
   position) are not supported in the current `parse_topic_value` helper. Each
   topic position accepts a single value or null (wildcard). Array support can
   be added when needed.

## Migration path

### For V1 callers (no change needed)

The `ChainIngestor::fetch_history()` implementation is unchanged. Callers that
use `LegacyConnectorAdapter` to wrap `EvmAdapter` continue to work through the
legacy address-filter path.

### For V2 callers (new)

1. Register an `IndexTarget` with the appropriate `TargetKind`:
   - `Wallet` + address for wallet activity
   - `Contract` + address for contract events
   - `TopicFilter` + `filter_spec` for arbitrary topic queries
2. Call `EvmAdapter::backfill(target, cursor, limit)`
3. Receive `IngestionBatch` with `RawTransaction` records
4. Store via `v2_repo` methods

### Verification of semantic fix

The key semantic change is testable:

- **Before**: `fetch_logs(wallet_address, ...)` → address filter → wrong for EOAs
- **After**: `fetch_wallet_erc20_logs(wallet, ...)` → topic filter → correct

Unit tests verify:
- Wallet target uses topic-based filtering (not address filter)
- Contract target uses address-based filtering
- TopicFilter target uses arbitrary topic array
- Unsupported target kinds are rejected
- Gas fee enrichment is preserved
- Native ETH value detection is preserved
- Cursor/checkpoint roundtrip works
- RawTransaction output has no wallet_address or user_id

## Test matrix

| Test | What it verifies |
|------|-----------------|
| `test_wallet_topic_filter_construction` | Wallet target builds correct topic + padded-address values |
| `test_contract_address_filter` | Contract target uses address parsing correctly |
| `test_topic_filter_spec_parsing` | TopicFilter target parses spec with null/string/missing topics |
| `test_unsupported_target_kind_rejected_by_connector` | Capabilities reject unsupported kinds |
| `test_unsupported_target_kinds_for_evm` | Validity matrix rejects Program/Market on EVM |
| `test_gas_fee_enrichment_in_raw_metadata` | Gas fields preserved in enriched metadata |
| `test_native_eth_value_in_enrichment` | Native ETH value detection preserved |
| `test_raw_transaction_is_target_agnostic` | No wallet_address/user_id in output |
| `test_cursor_roundtrip` | Cursor extraction and reconstruction |
| `test_parse_b256_valid` / `test_parse_b256_wrong_length` | B256 parsing correctness |
| `test_address_to_topic_pads_correctly` | Address→topic padding |
| `test_parse_topic_value_*` | Topic value parsing (null, hex, array rejection) |

## Files changed

- `adapters/src/evm.rs` — Major: V2 Connector impl, new fetch methods, unit tests
- `docs/phase2/p2-w3-evm-ingestion-handoff.md` — This document
