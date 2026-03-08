# P2-W2: Target Registration Handoff

## Summary

Added CLI and API target registration flows, allowing users to register arbitrary index targets (wallet, contract, program, account, topic_filter, market, pool, protocol) through both the HTTP API and CLI. This builds on the V2 domain types and connector validation from P2-W1.

## API Endpoints Added

All new routes are behind the existing `require_auth` middleware.

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/targets` | Register a new index target |
| `GET` | `/v1/targets` | List targets (with optional `kind`, `network`, `limit`, `offset` query params) |
| `GET` | `/v1/targets/:target_id` | Get a single target by UUID |
| `GET` | `/v1/networks` | List all available networks |
| `GET` | `/v1/networks/:network_id` | Get a single network by ID |

### POST /v1/targets Request Body

```json
{
  "kind": "wallet",
  "network": "solana-mainnet",
  "address": "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
  "filter_spec": null,
  "mode": "both",
  "label": "My Wallet"
}
```

- `kind` and `network` are required strings parsed via `FromStr`
- `mode` defaults to `"both"` if omitted
- Address is normalized per chain family (lowercase for EVM/Hyperliquid, passthrough for Solana)
- Returns 201 Created with the IndexTarget JSON on success
- Returns 400 for invalid kind, mode, unknown network, or validation failures
- Returns 409 Conflict for duplicate targets

## CLI Commands Added

| Command | Description |
|---------|-------------|
| `register-target` | Register a new index target |
| `list-targets` | List registered targets |
| `list-networks` | List available networks |

### register-target flags

- `--kind <KIND>` (required)
- `--network <NETWORK>` (required)
- `--address <ADDRESS>` (optional)
- `--filter-spec <JSON>` (optional)
- `--mode <MODE>` (default: "both")
- `--label <LABEL>` (optional)

All commands require `--db-url` to be set.

## Repository Changes

Added `list_index_targets(limit, offset)` method to `adapters/src/v2_repo.rs` for general paginated target listing without filters.

## Testing Summary

- **API tests (17 new)**: RegisterTargetRequest deserialization, validation rejection (missing address, invalid kind for family, missing filter_spec), kind/mode parsing, query param parsing, auth enforcement on all new endpoints, bad kind filter rejection, conflict error helper
- **CLI tests (8 new)**: register-target argument parsing (required/optional/defaults), list-targets parsing (no filter, with network, with kind), list-networks parsing, backward compatibility of existing ingest command
- **Adapters tests (1 new)**: list_index_targets method signature verification

All 401 tests pass across the workspace.

## Backward Compatibility

- Existing `POST /v1/ingest` and CLI `ingest` commands are unchanged
- The `ensure_wallet_target` wallet shorthand continues to work as before
- No existing tests, routes, or handlers were modified
- All changes are purely additive

## Files Changed

- `api/src/main.rs` - New routes, handlers, request types, tests
- `api/Cargo.toml` - Added `chrono` dependency
- `cli/src/main.rs` - New subcommands, handlers, tests
- `cli/Cargo.toml` - Added `chrono` dependency
- `adapters/src/v2_repo.rs` - Added `list_index_targets` method and test
- `docs/phase2/p2-w2-target-registration-handoff.md` - This document
