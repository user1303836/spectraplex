# P0-W3 Network Model Handoff

Status: **Frozen**
Date: 2026-03-07
Phase: P0-W3
Branch: `codex/p0-w3-network-model-spec`
Depends on: V2 Architecture RFC (P0-W1) Section 3.3, Target Model Spec (P0-W2)

This document is the tracked, reviewable artifact for the P0-W3 network model
contract. It captures the locked decisions from the full network model spec
(`design/NETWORK_MODEL.md`, which lives in the gitignored `design/` directory)
so that reviewers can validate the Phase 1 contract on GitHub without accessing
local-only design docs.

No new scope is introduced here. Every item below is drawn directly from the
frozen `design/NETWORK_MODEL.md`.

---

## 1. Frozen Network Registry

Phase 1 migrations (P1-W2) seed the `networks` table with exactly these rows.
Additional networks can be added later via data inserts without a schema change.

### 1.1 Network Table Schema

```sql
CREATE TABLE networks (
    id              TEXT PRIMARY KEY,       -- e.g. "solana-mainnet"
    chain_family    chain_family_enum NOT NULL,
    display_name    TEXT NOT NULL,
    is_testnet      BOOLEAN NOT NULL DEFAULT FALSE,
    finality_model  TEXT NOT NULL,          -- see Section 2
    block_time_ms   INTEGER,               -- NULL for blockless chains
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

RPC URLs are excluded. They are deployment-specific and belong in
`spectraplex.toml` or environment variables, not in the network registry.

### 1.2 Chain Family Enum

```sql
CREATE TYPE chain_family_enum AS ENUM ('solana', 'evm', 'hyperliquid');
```

Three values. Maps to the three protocol-level ingestion models Spectraplex
supports.

### 1.3 Solana Family Networks

| Network ID | Chain Family | Display Name | Testnet | Finality Model | Block Time |
|---|---|---|---|---|---|
| `solana-mainnet` | `solana` | Solana Mainnet | false | `probabilistic-slot` | 400ms |
| `solana-devnet` | `solana` | Solana Devnet | true | `probabilistic-slot` | 400ms |
| `solana-testnet` | `solana` | Solana Testnet | true | `probabilistic-slot` | 400ms |

### 1.4 EVM Family Networks

| Network ID | Chain Family | Display Name | Testnet | Finality Model | Block Time |
|---|---|---|---|---|---|
| `ethereum-mainnet` | `evm` | Ethereum Mainnet | false | `probabilistic-block` | 12000ms |
| `ethereum-sepolia` | `evm` | Ethereum Sepolia | true | `probabilistic-block` | 12000ms |
| `base-mainnet` | `evm` | Base Mainnet | false | `deterministic-block` | 2000ms |
| `base-sepolia` | `evm` | Base Sepolia | true | `deterministic-block` | 2000ms |
| `arbitrum-mainnet` | `evm` | Arbitrum One | false | `deterministic-block` | 250ms |
| `arbitrum-sepolia` | `evm` | Arbitrum Sepolia | true | `deterministic-block` | 250ms |
| `hyperevm-mainnet` | `evm` | HyperEVM Mainnet | false | `deterministic-block` | 2000ms |
| `hyperevm-testnet` | `evm` | HyperEVM Testnet | true | `deterministic-block` | 2000ms |

### 1.5 Hyperliquid Family Networks

| Network ID | Chain Family | Display Name | Testnet | Finality Model | Block Time |
|---|---|---|---|---|---|
| `hypercore-mainnet` | `hyperliquid` | HyperCore Mainnet | false | `instant` | N/A |
| `hypercore-testnet` | `hyperliquid` | HyperCore Testnet | true | `instant` | N/A |

### 1.6 Network ID Naming Convention

Pattern: `{chain_name}-{environment}`, all lowercase, ASCII alphanumeric and
hyphens only. Network IDs are stable and never change once assigned.

---

## 2. Four Finality Labels

The `finality_model` column is `TEXT`, not an enum, to allow future refinement
without a migration. Phase 1 uses exactly four values:

| Finality Model | Meaning | Applicable Chains |
|---|---|---|
| `probabilistic-slot` | Slot-based commitment levels (processed → confirmed → finalized). Slot skips possible. | Solana |
| `probabilistic-block` | Block-based with reorg risk. Finality via PoS gadget across multiple blocks. | Ethereum mainnet |
| `deterministic-block` | Sequencer-confirmed blocks. Final once posted to L1. Reorgs extremely unlikely. | Base, Arbitrum, HyperEVM |
| `instant` | API-confirmed events are final. No block/slot concept for ordering. | HyperCore |

The V2 Architecture RFC (P0-W1) originally used three finality labels. P0-W3
refined them to four by distinguishing `probabilistic-block` (Ethereum L1, real
reorg concern) from `deterministic-block` (L2 rollups, sequencer-final). The RFC
has been updated in `design/V2_ARCHITECTURE_RFC.md` to use the four-value set.

---

## 3. Six Required Worked Examples

Each example shows a `networks` row and a representative V2 checkpoint cursor.

### 3.1 Solana Mainnet

**Network row:** `solana-mainnet`, chain family `solana`, finality `probabilistic-slot`, block time 400ms.

**RPC checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "solana-mainnet",
  "source": "rpc",
  "cursor": {
    "last_signature": "5VERv8NMhzgC4...",
    "last_slot": 298412345,
    "commitment": "finalized"
  }
}
```

**gRPC checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "solana-mainnet",
  "source": "grpc",
  "cursor": {
    "last_slot": 298412400,
    "commitment": "confirmed"
  }
}
```

RPC and gRPC sources use different pagination primitives (signature vs slot) and
have independent checkpoint rows keyed by `(target_id, network, source)`.

### 3.2 Ethereum Mainnet

**Network row:** `ethereum-mainnet`, chain family `evm`, finality `probabilistic-block`, block time 12000ms.

**RPC checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "ethereum-mainnet",
  "source": "rpc",
  "cursor": {
    "last_block": 21500000,
    "last_block_hash": "0x4e3a3754410177e6937ef1f84bba68ea139e8d1a..."
  }
}
```

Reorg detection uses `last_block_hash` comparison on checkpoint load.

### 3.3 Base Mainnet

**Network row:** `base-mainnet`, chain family `evm`, finality `deterministic-block`, block time 2000ms.

**RPC checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "base-mainnet",
  "source": "rpc",
  "cursor": {
    "last_block": 25000000,
    "last_block_hash": "0xabc123..."
  }
}
```

Uses the same EVM connector as Ethereum. `deterministic-block` finality means
reorg detection is optional but `last_block_hash` is stored defensively.

### 3.4 Arbitrum Mainnet

**Network row:** `arbitrum-mainnet`, chain family `evm`, finality `deterministic-block`, block time 250ms.

**RPC checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "arbitrum-mainnet",
  "source": "rpc",
  "cursor": {
    "last_block": 280000000,
    "last_block_hash": "0xdef456..."
  }
}
```

Fast block times (~250ms) may require larger `block_chunk` sizes for efficient
backfill. That is a connector configuration concern, not a network model concern.

### 3.5 HyperCore Mainnet

**Network row:** `hypercore-mainnet`, chain family `hyperliquid`, finality `instant`, block time NULL.

**REST checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "hypercore-mainnet",
  "source": "rest",
  "cursor": {
    "last_timestamp_ms": 1700000000001,
    "last_fill_tid": 987654,
    "data_types_covered": ["fills", "funding", "ledger_updates"]
  }
}
```

Timestamp ordering (millisecond precision) plus fill trade ID for sub-millisecond
deduplication.

### 3.6 HyperEVM Mainnet

**Network row:** `hyperevm-mainnet`, chain family `evm`, finality `deterministic-block`, block time 2000ms.

**RPC checkpoint cursor:**
```json
{
  "target_id": "<uuid>",
  "network": "hyperevm-mainnet",
  "source": "rpc",
  "cursor": {
    "last_block": 5000000,
    "last_block_hash": "0x789abc..."
  }
}
```

HyperEVM is an EVM chain. It uses the standard EVM connector with a HyperEVM RPC
URL. See Section 4 for the routing split.

---

## 4. HyperCore vs HyperEVM Routing Split

HyperCore and HyperEVM are separate ingestion models despite being part of the
same Hyperliquid ecosystem.

| Property | HyperCore | HyperEVM |
|---|---|---|
| Chain family | `hyperliquid` | `evm` |
| Network ID | `hypercore-mainnet` | `hyperevm-mainnet` |
| Protocol model | Order book / clearinghouse | EVM smart contracts |
| Ingestion method | REST API + WebSocket | `eth_getLogs` + `eth_getBlockByNumber` |
| Finality | Instant (API-confirmed) | Deterministic block (sequencer-confirmed) |
| Checkpoint model | Timestamp + trade ID cursor | Block number + block hash cursor |

**Connector routing is determined by `chain_family`, not by ecosystem:**

```
Target on hypercore-mainnet → chain_family = hyperliquid → HyperliquidRestConnector / HyperliquidWsConnector
Target on hyperevm-mainnet  → chain_family = evm         → EvmConnector
```

A Hyperliquid user wanting complete coverage creates targets on both networks.
Cross-chain event correlation (e.g. bridging between HyperCore and HyperEVM) is
a downstream Silver/Gold layer concern, not a Bronze ingestion concern.

---

## 5. Checkpoint Cursor Shapes (Summary)

| Chain Family | Cursor Shape | Primary Resume Field | Reorg Detection |
|---|---|---|---|
| `solana` | `{ last_signature, last_slot, commitment }` | `last_signature` (RPC), `last_slot` (gRPC) | Not needed at confirmed+; slot-skip handling via gRPC `from_slot` |
| `evm` | `{ last_block, last_block_hash, last_log_index }` | `last_block` | `last_block_hash` comparison on checkpoint load |
| `hyperliquid` | `{ last_timestamp_ms, last_fill_tid, data_types_covered }` | `last_timestamp_ms` | Not needed (instant finality) |

---

## 6. Phase 1 Legacy Checkpoint Migration Guidance

This section provides concrete guidance for migrating existing
`indexer_checkpoints` rows to V2 `checkpoints` rows during the dual-write
transition (P1-W4). It corrects three issues identified in local review.

### 6.1 Current Legacy Schema

From `migrations/20260217010000_add_indexer_checkpoints.sql`:

```sql
CREATE TABLE indexer_checkpoints (
    chain chain_enum NOT NULL,             -- 'solana', 'hyperliquid', 'ethereum'
    wallet_address TEXT NOT NULL,
    last_signature TEXT,
    last_slot BIGINT,
    last_timestamp BIGINT,
    updated_at TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (chain, wallet_address)
);
```

From `migrations/20260218000000_add_last_block_to_checkpoints.sql`:

```sql
ALTER TABLE indexer_checkpoints ADD COLUMN last_block BIGINT;
```

### 6.2 Solana: Persisted Resume Cursor Semantics

**Important correction from earlier draft:** The Solana gRPC adapter
(`adapters/src/solana_grpc.rs`) uses an in-memory `SlotCheckpoint` (an
`AtomicU64` at lines 51-75) for `from_slot` reconnection. The adapter
itself does not call `repo.save_checkpoint()` with slot-based state.

However, the CLI ingest path (`cli/src/main.rs`, lines 147-155) calls
`SolanaGrpcAdapter::fetch_history()` and then passes the resulting
transactions through `build_checkpoint()` (`adapters/src/repo.rs`,
lines 42-78) and `save_transactions_and_checkpoint()` (lines 607-621).
This means **gRPC-derived Solana `indexer_checkpoints` rows CAN exist**
in the database. These rows have the same schema as RPC-derived rows:

- `last_signature`: set to the latest transaction hash (from
  `build_checkpoint`, line 73)
- `last_slot`: extracted from `raw_metadata["slot"]` (lines 54-59);
  present on BOTH RPC and gRPC-derived Solana transactions because both
  adapter outputs include `"slot"` in their `raw_metadata`
- `last_timestamp`: set to the latest transaction timestamp (line 76)

**The legacy `indexer_checkpoints` schema cannot distinguish RPC-derived
from gRPC-derived rows.** Both sources produce identical checkpoint
shapes through `build_checkpoint()`.

**Resume cursor semantics by adapter:**

| Adapter | Persisted field used for resume | How resume works |
|---|---|---|
| Solana RPC (`adapters/src/solana.rs`, lines 38-40) | `last_signature` | Passed as the `until` parameter to `getSignaturesForAddress`; the RPC returns transactions newer than this signature. |
| Solana gRPC via CLI (`cli/src/main.rs`, lines 149-152) | `last_slot` | CLI reads `checkpoint.last_slot`, calls `adapter.checkpoint().update(slot as u64)` to seed the in-memory `SlotCheckpoint`, then `fetch_history` uses `self.checkpoint.get()` as `from_slot`. |

**Why `last_signature` is the only authoritative persisted resume cursor:**

- `last_signature` is the **only** persisted cursor that an adapter reads
  directly for pagination. The Solana RPC adapter reads
  `checkpoint.last_signature` (line 38-40) and passes it to the RPC as
  the `until` parameter.
- `last_slot` is used by the CLI gRPC path indirectly (CLI code seeds the
  in-memory `SlotCheckpoint`), but it is **not** a V2 gRPC checkpoint
  equivalent: it carries no `commitment`-level metadata, may not reflect
  the gRPC adapter's actual in-memory slot state at process exit, and is
  present on RPC-derived rows too.
- Because RPC and gRPC-derived rows are indistinguishable in the legacy
  schema, the migration defaults all legacy Solana rows to
  `source = "rpc"` with `last_signature` as the primary cursor. This is
  safe for RPC resume.
- Legacy gRPC users will need to create fresh V2 gRPC checkpoints when
  Phase 2 gRPC connectors start persisting `(last_slot, commitment)` state
  directly to the `checkpoints` table.

**Migration mapping for Solana:**

| Old field | V2 cursor field | Notes |
|---|---|---|
| `last_signature` | `cursor.last_signature` | Authoritative RPC resume cursor; present on all legacy Solana rows regardless of original adapter |
| `last_slot` | `cursor.last_slot` | Informational only; present on all legacy Solana rows; not promoted to V2 gRPC cursor state |
| N/A | `source` | Default `"rpc"` for all legacy Solana rows; gRPC-derived rows cannot be reliably distinguished |
| N/A | `cursor.commitment` | Not tracked in legacy schema; omit or default to `"finalized"` (RPC `getSignaturesForAddress` returns finalized by default) |

### 6.3 HyperCore: Timestamp Precision Caveat

**Caveat (corrected from earlier draft):** The current Hyperliquid adapter
(`adapters/src/hyperliquid.rs`, lines 182-185) stores `last_timestamp` in
the `indexer_checkpoints` table as **seconds** (epoch seconds). The
conversion happens at line 200: `timestamp: (fill.time / 1000) as i64`,
which truncates milliseconds.

The V2 checkpoint cursor uses `last_timestamp_ms` (milliseconds). A naive
migration that multiplies by 1000 (`last_timestamp * 1000`) would produce
a value with **false millisecond precision**: the resulting timestamp would
always end in `000`, which is not the actual sub-second time of the last
event.

**Impact on resume semantics:**

- The current adapter resumes with `resume_ms = (checkpoint.last_timestamp * 1000) + 1` (line 184).
- After migration, if `last_timestamp_ms = last_timestamp * 1000`, the
  resume point becomes `last_timestamp_ms + 1`.
- This is equivalent to the current behavior and does **not** cause data
  loss.
- However, events that occurred in the same second as the checkpoint but
  after the truncated timestamp boundary will be **re-fetched** on resume.

**Required replay/dedupe semantics for Phase 1:**

1. The migration sets `cursor.last_timestamp_ms = last_timestamp * 1000`.
   The value is understood to have second-precision granularity rounded
   down to the millisecond boundary.
2. On first resume after migration, the connector must be prepared to see
   duplicate events from the final second of the previous run. The
   existing deduplication on `(network, tx_hash)` in `raw_transactions`
   handles this for transaction-level records.
3. For fills specifically, `last_fill_tid` is **not available** in legacy
   checkpoints. The migrated cursor omits `last_fill_tid`. The Phase 2
   HyperCore connector should use timestamp-only deduplication until a
   `last_fill_tid` is captured in the first post-migration checkpoint
   write.
4. New V2 checkpoint writes from the Phase 2 connector will store true
   millisecond timestamps directly from the API response (`fill.time`,
   `funding.time`, `update.time`), so the precision loss is a one-time
   migration artifact.

**Migration mapping for HyperCore:**

| Old field | V2 cursor field | Notes |
|---|---|---|
| `last_timestamp` | `cursor.last_timestamp_ms` | Multiply by 1000; understood as second-precision |
| N/A | `cursor.last_fill_tid` | Not available in legacy; omit |
| N/A | `source` | Always `"rest"` for legacy HyperCore rows |
| N/A | `cursor.data_types_covered` | Not tracked in legacy; omit or default to `["fills", "funding", "ledger_updates"]` |

### 6.4 EVM: Network Reclassification Caveat

**Caveat (carried from spec):** The current `Chain::Ethereum` variant
(`core/src/models.rs`, lines 6-10) is used for all EVM-compatible chains.
The repository maps it to the database string `"ethereum"`
(`adapters/src/repo.rs`, lines 4-9).

The migration defaults all legacy `chain = 'ethereum'` rows to
`network = 'ethereum-mainnet'`. This is correct for deployments that only
indexed Ethereum mainnet data via `Chain::Ethereum`.

**If a deployment was indexing Base, Arbitrum, or other EVM chain data
under `Chain::Ethereum`**, those rows will be incorrectly assigned to
`ethereum-mainnet`. This requires manual reclassification.

**Phase 1 must define the exact override mechanism before migration code
ships.** Options under consideration (to be resolved in P1-W2):

1. **Migration-time environment variable**: e.g.
   `SPECTRAPLEX_LEGACY_EVM_NETWORK=base-mainnet` forces all legacy
   Ethereum rows to a specific network. Simple but only works for
   single-network deployments.
2. **Migration-time configuration map**: a TOML or JSON map from
   `(chain, wallet_address)` to `network` for targeted reclassification.
3. **Post-migration reclassification CLI command**: a new CLI command to
   reassign `network` on existing rows by wallet address or block range.

The choice is deferred to P1-W2. This handoff documents the requirement
so it is not lost.

**Migration mapping for EVM:**

| Old field | V2 cursor field | Notes |
|---|---|---|
| `last_block` | `cursor.last_block` | Direct mapping |
| N/A | `cursor.last_block_hash` | Not tracked in legacy; omit (reorg detection starts fresh from V2) |
| N/A | `source` | Always `"rpc"` for legacy EVM rows |
| N/A | `network` | Default `"ethereum-mainnet"` unless overridden; see caveat above |

### 6.5 Legacy-to-V2 Default Source Summary

| Legacy `chain` value | V2 `chain_family` | V2 default `network` | V2 default `source` | Rationale |
|---|---|---|---|---|
| `solana` | `solana` | `solana-mainnet` | `rpc` | Legacy Solana checkpoints may come from either the RPC or gRPC CLI path but are indistinguishable in the legacy schema; `last_signature` is the authoritative persisted resume cursor (Section 6.2) |
| `hyperliquid` | `hyperliquid` | `hypercore-mainnet` | `rest` | All persisted HyperCore checkpoints come from the REST adapter |
| `ethereum` | `evm` | `ethereum-mainnet` | `rpc` | All persisted EVM checkpoints come from the RPC adapter; may need network reclassification (Section 6.4) |

---

## 7. Decisions Locked By This Handoff

These decisions are frozen. Phase 1+ work packets should reference them, not
reopen them.

| Decision | Choice | Rationale |
|---|---|---|
| Three chain families | `solana`, `evm`, `hyperliquid` | Maps to three protocol-level ingestion models |
| HyperEVM is `evm`, not a fourth family | `hyperevm-mainnet` in chain family `evm` | HyperEVM uses standard EVM JSON-RPC |
| HyperCore is `hyperliquid` | `hypercore-mainnet` in chain family `hyperliquid` | HyperCore uses REST/WS, not EVM primitives |
| Network IDs are `{chain}-{env}` lowercase | e.g. `solana-mainnet`, `hypercore-mainnet` | Stable, readable, collision-free |
| Four finality models | `probabilistic-slot`, `probabilistic-block`, `deterministic-block`, `instant` | Distinguishes L1 reorg risk from L2 sequencer finality |
| Checkpoint cursor is JSONB | Per-family cursor shapes in Section 5 | Different chains need different cursor fields |
| `finality_model` is TEXT, not enum | Allows future refinement without migration | New chains may need new finality semantics |
| RPC URLs excluded from `networks` table | Deployment-specific config belongs in spectraplex.toml or env vars | Network registry stores protocol facts |
| HyperCore network ID is `hypercore-mainnet` | Not `hyperliquid-mainnet` | Avoids ambiguity with the broader ecosystem |
| `block_time_ms` is nullable | HyperCore has no block concept | Prevents meaningless zero values |

---

## 8. Terminology Cross-Reference

This section confirms alignment with the other Phase 0 specs.

### 8.1 V2 Architecture RFC (P0-W1) Alignment

**Taxonomy and finality alignment (verified):**

- `chain_family_enum` values: `solana, evm, hyperliquid` — **matches**
- Network ID examples: `solana-mainnet`, `ethereum-mainnet`, `base-mainnet` — **matches**
- Finality model labels: four-value set adopted from P0-W3 — **matches** (RFC Section 3.3 updated)
- HyperEVM routing: EVM connector with HyperEVM network — **matches**
- `networks` table schema: `rpc_url` removed, `block_time_ms` added — **matches** (RFC Section 3.3 updated)

**Checkpoint cursor shapes (partial alignment — P0-W3 extends the RFC):**

- The RFC Section 3.4 documents baseline cursor shapes: Solana
  `{ last_signature, last_slot }`, EVM `{ last_block, last_block_hash }`,
  Hyperliquid `{ last_timestamp, last_fill_tid }`.
- P0-W3 extends these with additional fields: `commitment` (Solana),
  `last_log_index` (EVM), `data_types_covered` (Hyperliquid).
- P0-W3 renames the Hyperliquid field `last_timestamp` to
  `last_timestamp_ms` to reflect that the V2 cursor stores millisecond
  precision directly (Section 6.3 documents the legacy second-precision
  caveat).
- The RFC's Section 3.4 inline JSONB comment (`{ last_signature,
  last_slot, last_block, last_timestamp, ... }`) and the Hyperliquid
  cursor field names have **not been updated** to reflect the P0-W3
  refinements. This is a documentation gap in `design/V2_ARCHITECTURE_RFC.md`,
  not a semantic conflict.
- **The authoritative cursor field definitions for Phase 1 are in this
  handoff (Section 5), not in the RFC.**

### 8.2 Target Model Spec (P0-W2) Alignment

- `index_targets.network` examples: `solana-mainnet`, `ethereum-mainnet`, `base-mainnet` — **matches**
- `index_targets.chain_family` values: `solana, evm, hyperliquid` — **matches**
- HyperEVM contract target routing: EVM chain family with `hyperevm-mainnet` — **matches**
- HyperCore target examples use `hypercore-mainnet` — **matches** (corrected from `hyperliquid-mainnet` in P0-W3)

---

## 9. Open Questions For Follow-On Packets

| Question | Deferred To |
|---|---|
| EVM legacy network reclassification override mechanism | P1-W2 |
| Exact `block_chunk` tuning per network for EVM backfill | P2-W3 |
| Reorg handling depth and reprocessing strategy for Ethereum mainnet | P2-W3 |
| WebSocket reconnection and gap-fill protocol for HyperCore | P2-W5 |
| Whether Solana gRPC checkpoint should also store `last_signature` for cross-source reconciliation | P2-W4 |
| Additional EVM networks beyond the initial set (Optimism, Polygon, etc.) | Phase 1+ via data insert |
| Commitment-level configuration per target | P2-W4 |
| Cross-chain event correlation between HyperCore and HyperEVM | Phase 3+ |
