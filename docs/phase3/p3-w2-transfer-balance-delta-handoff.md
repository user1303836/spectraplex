# P3-W2: Transfer and Balance-Delta Datasets — Handoff

## Summary

This packet adds two new Silver datasets — `token_transfers` and `native_balance_deltas` — along with Materializer implementations for each supported chain family and repository methods for persistence and query.

## Schema

### token_transfers

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| raw_transaction_id | UUID FK → raw_transactions | Nullable during transition |
| network | TEXT NOT NULL | Generic chain identifier |
| token_address | TEXT NOT NULL | Mint (Solana) or contract (EVM) or symbol (HL) |
| token_symbol | TEXT | Human-readable if resolvable |
| from_address | TEXT NOT NULL | Sender address (generic, not wallet-specific) |
| to_address | TEXT NOT NULL | Receiver address (generic, not wallet-specific) |
| amount | NUMERIC NOT NULL | Normalized by decimals |
| decimals | INT NOT NULL | Token decimal places |
| dataset_version_id | UUID FK → dataset_versions | Nullable during transition |
| created_at | TIMESTAMPTZ NOT NULL | Default NOW() |

**Indexes:** from_address, to_address, raw_transaction_id, network, dataset_version_id.
**Dedup:** Unique on (raw_transaction_id, from_address, to_address, token_address, amount) WHERE raw_transaction_id IS NOT NULL.

### native_balance_deltas

| Column | Type | Notes |
|--------|------|-------|
| id | UUID PK | |
| raw_transaction_id | UUID FK → raw_transactions | Nullable during transition |
| network | TEXT NOT NULL | Generic chain identifier |
| account_address | TEXT NOT NULL | Account whose balance changed |
| native_token | TEXT NOT NULL | e.g. "SOL", "ETH", "USDC" |
| pre_balance | NUMERIC NOT NULL | Balance before tx |
| post_balance | NUMERIC NOT NULL | Balance after tx |
| delta | NUMERIC NOT NULL | post - pre |
| is_fee_payer | BOOLEAN NOT NULL | True if account paid tx fees |
| dataset_version_id | UUID FK → dataset_versions | Nullable during transition |
| created_at | TIMESTAMPTZ NOT NULL | Default NOW() |

**Indexes:** account_address, raw_transaction_id, network, dataset_version_id.
**Dedup:** Unique on (raw_transaction_id, account_address) WHERE raw_transaction_id IS NOT NULL.

## Materializer Implementations

| Dataset | Chain Family | Struct | Parser Hash | Notes |
|---------|-------------|--------|-------------|-------|
| TokenTransfers | Solana | `SolanaTokenTransferMaterializer` | `sha256:solana_token_transfers_v1_d4e9f1a7` | Extracts SPL token movements from pre/post token balances |
| TokenTransfers | EVM | `EvmTokenTransferMaterializer` | `sha256:evm_token_transfers_v1_a1c5e7b9` | Decodes ERC-20 Transfer events from log topics/data |
| TokenTransfers | Hyperliquid | `HyperliquidTokenTransferMaterializer` | `sha256:hl_token_transfers_v1_e7f2a4d8` | Models deposits/withdrawals as USDC transfers |
| NativeBalanceDeltas | Solana | `SolanaNativeBalanceDeltaMaterializer` | `sha256:solana_native_deltas_v1_b2c3a8f6` | Extracts SOL balance changes per account; flags fee payer |
| NativeBalanceDeltas | Hyperliquid | `HyperliquidNativeBalanceDeltaMaterializer` | `sha256:hl_native_deltas_v1_f3d6c9a2` | USDC balance changes from fills and funding |

All 8 materializers (3 ledger + 5 new) have globally distinct parser_hash values and valid descriptors.

## Parser Extraction Functions

| Function | Chain | Input | Output |
|----------|-------|-------|--------|
| `extract_solana_token_transfers()` | Solana | raw_metadata (EncodedConfirmedTransactionWithStatusMeta) | Vec<TokenTransfer> |
| `extract_solana_native_balance_deltas()` | Solana | raw_metadata (EncodedConfirmedTransactionWithStatusMeta) | Vec<NativeBalanceDelta> |
| `extract_evm_token_transfers()` | EVM | raw_metadata (log with topics/data/address) | Vec<TokenTransfer> |
| `extract_hyperliquid_token_transfers()` | Hyperliquid | raw_metadata (ledger_update with deposit/withdraw delta) | Vec<TokenTransfer> |
| `extract_hyperliquid_native_balance_deltas()` | Hyperliquid | raw_metadata (fill/funding) | Vec<NativeBalanceDelta> |

## Repository Methods

| Method | Table | Operation |
|--------|-------|-----------|
| `save_token_transfers()` | token_transfers | Bulk insert (chunked by V2_BATCH_SIZE=500) |
| `get_token_transfers_by_address()` | token_transfers | Query by from_address OR to_address with LIMIT/OFFSET |
| `get_token_transfers_by_raw_tx()` | token_transfers | Query by raw_transaction_id |
| `save_native_balance_deltas()` | native_balance_deltas | Bulk insert (chunked by V2_BATCH_SIZE=500) |
| `get_native_balance_deltas_by_account()` | native_balance_deltas | Query by account_address with LIMIT/OFFSET |
| `get_native_balance_deltas_by_raw_tx()` | native_balance_deltas | Query by raw_transaction_id |

## Deferred Decisions

### EVM Native Balance Deltas — Deferred

EVM native balance deltas require trace API data (`debug_traceTransaction` or `trace_transaction`) which is not yet stored in Bronze `raw_transactions`. The current Bronze model stores log-level data and transaction-level metadata (value, gas_used, effective_gas_price), but this is insufficient for accurate native balance deltas across all accounts because:

- The `value` field only captures the direct ETH transfer between from/to.
- Internal transactions (contract-to-contract ETH transfers) are invisible without traces.
- Gas refunds and self-destructs also affect balances.

**Resolution path:** When Bronze gains trace API support (e.g. a `raw_evm_traces` table), implement `EvmNativeBalanceDeltaMaterializer`. The `DatasetName::NativeBalanceDeltas` enum variant and table already exist; only the materializer and extraction function are missing for EVM.

## Design Choices

1. **Generic addresses, not wallet-specific.** Both tables use `from_address`/`to_address` and `account_address` — never `wallet_address` or `user_id`. This supports contract, program, and protocol target types.

2. **Nullable FKs for transition safety.** `raw_transaction_id` and `dataset_version_id` are nullable so records can be created before full Bronze/versioning integration is complete.

3. **Dedup indexes use WHERE NOT NULL guards.** Records without a raw_transaction_id skip the uniqueness constraint, allowing flexibility during the transition period.

4. **Hyperliquid native token is USDC.** On Hyperliquid, the settlement currency is USDC, so `native_token` is "USDC" rather than a gas token. Pre/post balances are zero when absolute values are unavailable from fill/funding data — only the delta is populated.

## Downstream Packet Dependencies

- **P3-W3 (Event and instruction datasets):** Can proceed independently; uses the same Materializer trait and migration patterns.
- **P3-W5 (Ledger as derived materialization):** Can now consume token_transfers and native_balance_deltas as inputs instead of re-deriving from Bronze.
- **P3-W6 (Completeness metadata):** Should track completeness for token_transfers and native_balance_deltas alongside other datasets.
- **P4-W1 (Dataset query API):** These new Silver tables are ready for dataset-oriented query endpoints.

## Test Coverage

- 574 tests pass (36 new tests added across core, adapters, and integration)
- Domain struct serde roundtrips and no-wallet-field assertions
- Materializer trait contract verification for all 5 new materializers
- Globally distinct parser_hash validation across all 8 materializers
- Query builder correctness (param counts, ON CONFLICT clauses)
- Repository method Send-ability checks
- Extraction function edge cases (empty data, non-matching types, zero amounts)
