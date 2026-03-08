# P0-W2 Target Model Spec Handoff

Status: **Frozen**
Date: 2026-03-08
Phase: P0-W2
Branch: `codex/p0-w2-target-model-spec`
Depends on: V2 Architecture RFC (P0-W1) Section 3.2
Downstream dependents: P0-W4 (Rollout Plan), P1-W1 (Core Types), P1-W2 (Migrations), P2-W1 through P2-W5 (Connector and Ingestion)

This document is the tracked, reviewable artifact for the P0-W2 Target Model
Spec. It captures the locked decisions from the full spec (`design/TARGET_MODEL.md`,
which lives in the gitignored `design/` directory) so that reviewers can validate
the target model contract on GitHub without accessing local-only design docs.

No new scope is introduced here. Every item below is drawn directly from the
frozen `design/TARGET_MODEL.md` (926 lines).

---

## 1. Reference Schema (from P0-W1 RFC Section 3.2)

The `index_targets` table shape is locked by the V2 Architecture RFC. This spec
fills in the per-kind semantics, filter_spec details, validity matrix, and
uniqueness rules that the RFC explicitly deferred to P0-W2.

```
index_targets
  id            UUID PRIMARY KEY
  kind          target_kind_enum NOT NULL   -- wallet, contract, program, account, topic_filter, market, pool, protocol
  network       TEXT NOT NULL               -- solana-mainnet, ethereum-mainnet, base-mainnet, etc.
  chain_family  chain_family_enum NOT NULL  -- solana, evm, hyperliquid
  address       TEXT                        -- primary address for simple target types
  filter_spec   JSONB                       -- structured filter for complex targets
  mode          target_mode_enum NOT NULL   -- backfill, stream, both
  label         TEXT                        -- human-readable label
  owner_id      UUID                        -- optional tenant/user association
  created_at    TIMESTAMPTZ DEFAULT NOW()
  updated_at    TIMESTAMPTZ DEFAULT NOW()
```

Target matching uses the `target_matches` join table (P0-W1 Section 3.2) so that
a single raw transaction can link to multiple targets.

---

## 2. Target Kinds: Locked Semantics

Eight target kinds are locked by the V2 Architecture RFC. This spec defines the
precise identity semantics, match criteria, and ingestion strategy for each.

### 2.1 wallet

Tracks activity for an externally owned address: an EOA on EVM chains, a keypair
pubkey on Solana, or a user address on Hyperliquid.

**Identity:** `address` holds the wallet's canonical address.
- EVM: checksummed hex (e.g. `0x742d35Cc6634C0532925a3b844Bc9e7595f2bD18`), stored lowercase
- Solana: base58-encoded pubkey (e.g. `DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy`)
- Hyperliquid: HyperCore user address (EVM hex format), stored lowercase

**Match criteria:** A raw transaction matches when the wallet address appears as a
signer, sender, receiver, fee payer, or token account owner.

**Ingestion per chain family:**

| Chain Family | Backfill | Stream |
|---|---|---|
| Solana | `getSignaturesForAddress` + `getTransaction` | Yellowstone gRPC `account_include` |
| EVM | ERC-20 Transfer logs by `from`/`to` topics + native transfer scanning | `eth_subscribe` (future) |
| Hyperliquid | REST: `fetch_user_fills`, `fetch_user_funding`, `fetch_user_ledger_updates` | WebSocket user subscription |

**filter_spec (optional):**
```json
{
  "asset_filter": ["SOL", "USDC"],
  "min_amount": "1.0",
  "directions": ["inbound", "outbound", "self"]
}
```

These narrow downstream queries, not ingestion. The connector fetches all activity;
filtering happens at the matching or query layer.

### 2.2 contract

Tracks events emitted by a specific smart contract on an EVM chain.

**Identity:** `address` holds the contract address in checksummed hex, stored lowercase.

**Match criteria:** A raw transaction matches when it contains a log emitted by
the contract address. If `event_signatures` or `topic_filters` are specified in
`filter_spec`, only matching logs cause a match.

**Valid on:** EVM only. Solana uses `program`. Hyperliquid HyperEVM routes through
EVM chain family with network `hyperevm-mainnet`.

**filter_spec (optional):**
```json
{
  "event_signatures": ["Transfer(address,address,uint256)"],
  "topic_filters": {
    "topic1": "0x000000000000000000000000742d35cc6634c0532925a3b844bc9e7595f2bd18"
  }
}
```

### 2.3 program

Tracks transactions that invoke a specific Solana program.

**Identity:** `address` holds the program ID as base58 pubkey
(e.g. `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`).

**Match criteria:** A raw transaction matches when the program ID appears in the
transaction's account keys. If `instruction_discriminators` is specified, the
transaction must contain a matching instruction.

**Valid on:** Solana only. EVM uses `contract`.

**filter_spec (optional):**
```json
{
  "instruction_discriminators": ["0x03", "0x09"],
  "include_cpi": true
}
```

### 2.4 account

Tracks transactions that read or write a specific on-chain account. Distinct from
`wallet` because an account target does not imply signer or owner semantics. Most
relevant on Solana, where PDAs, token accounts, data accounts, and vault accounts
are valid non-wallet indexing targets.

**Identity:** `address` holds the account pubkey in base58.

**Match criteria:** A raw transaction matches when the account pubkey appears in
the transaction's account keys. If `access_types` restricts to `write`, only
transactions where the account is in the writable set match.

**Valid on:** Solana (primary). EVM (limited, with warning to use `wallet` or
`contract` instead). Not valid on Hyperliquid.

**filter_spec (optional):**
```json
{
  "access_types": ["write", "read"],
  "associated_program": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
}
```

**Why `account` is distinct from `wallet`:** On Solana, a PDA cannot sign
transactions. Wallet targets generate match reasons like "signer", "fee_payer".
Account targets generate match reasons like "account_key", "writable_account".
The ingestion primitive is the same (`getSignaturesForAddress` or gRPC
`account_include`), but downstream interpretation differs.

### 2.5 topic_filter

Tracks EVM log entries matching arbitrary topic criteria, independent of which
contract emitted them.

**Identity:** `address` is null. The target is defined entirely by `filter_spec`.

**Match criteria:** A raw transaction matches when it contains at least one log
whose topics satisfy the filter. If `address_filter` is present, the log must also
be emitted by one of the listed addresses.

**Valid on:** EVM only. Solana does not have indexed topic-based events. Hyperliquid
is not topic-based.

**filter_spec (required):**
```json
{
  "topics": [
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
    null,
    "0x000000000000000000000000742d35cc6634c0532925a3b844bc9e7595f2bd18"
  ],
  "address_filter": [
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    "0xdAC17F958D2ee523a2206206994597C13D831ec7"
  ]
}
```

`topics` follows `eth_getLogs` semantics exactly: up to 4 positions, each a hex
hash, null (match any), or array of hashes (match any of set).

### 2.6 market

Tracks activity for a specific trading market. Currently applicable only to
Hyperliquid.

**Identity:** `address` holds the market symbol (e.g. `ETH` for ETH-PERP).

**Match criteria:** A raw transaction matches when it represents activity in the
specified market (fill, funding event, or liquidation).

**Valid on:** Hyperliquid only. Solana DEX markets use `program`, `account`, or
`pool`. EVM DEX markets use `contract` or `pool`.

**filter_spec (optional):**
```json
{
  "data_types": ["fills", "funding", "liquidations"],
  "min_size": "1.0"
}
```

### 2.7 pool

Tracks activity for a DeFi liquidity pool, identified by its on-chain address.

**Identity:** `address` holds the pool address (checksummed hex for EVM, stored
lowercase; base58 for Solana).

**Match criteria:**
- **EVM:** log emitted by the pool address; optional `event_types` narrowing
- **Solana:** pool account appears in the transaction's account keys

**Valid on:** EVM and Solana. Not valid on Hyperliquid (use `market` for HyperCore).

**filter_spec (optional):**
```json
{
  "protocol": "uniswap_v3",
  "token_pair": ["USDC", "WETH"],
  "event_types": ["Swap", "Mint", "Burn"]
}
```

**Relationship to `contract` and `account`:** A `pool` is a semantic
specialization. On EVM it behaves like `contract` (log-based). On Solana it
behaves like `account` (account-key-based). The `pool` kind exists because it
carries pool-specific metadata and signals to normalizers that raw data should be
interpreted as pool activity (swaps, liquidity events).

### 2.8 protocol

Tracks activity across a protocol's known set of contracts, programs, or accounts.
A composite target that may span multiple addresses and potentially multiple
networks within the same chain family.

**Identity:** `address` is null. Defined entirely by `filter_spec`.

**Match criteria:** A raw transaction matches when it involves any address listed
in the protocol's `filter_spec` for the target's network. Match reasons include
the specific address and role that triggered the match.

**Valid on:** EVM and Solana. Not applicable on Hyperliquid (HyperCore is a single
protocol; use `wallet` and `market` targets).

**filter_spec (required):**
```json
{
  "name": "aave_v3",
  "addresses": {
    "ethereum-mainnet": [
      { "address": "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2", "role": "pool", "label": "Aave V3 Pool" },
      { "address": "0x2f39d218133afab8f2b819b1066c7e434ad94e9e", "role": "registry", "label": "Pool Addresses Provider" }
    ]
  }
}
```

**Implementation note:** A protocol target registration may create child targets
per address for operational purposes. The control plane expands a protocol target
into individual connector subscriptions per address. This expansion is an
implementation detail; the external API presents a single protocol target.

---

## 3. Per-Chain-Family Validity Matrix

This matrix defines which target kinds are valid for each chain family.

| Target Kind | Solana | EVM | Hyperliquid |
|---|---|---|---|
| **wallet** | Valid | Valid | Valid |
| **contract** | Not valid | Valid | Not valid |
| **program** | Valid | Not valid | Not valid |
| **account** | Valid | Limited | Not valid |
| **topic_filter** | Not valid | Valid | Not valid |
| **market** | Not valid | Not valid | Valid |
| **pool** | Valid | Valid | Not valid |
| **protocol** | Valid | Valid | Not valid |

### Key validity rationale

**wallet** is valid on all three families because each has native primitives for
fetching activity by address/user.

**contract vs program:** These are the chain-specific equivalents. EVM contracts
emit logs; Solana programs are invoked via instructions. Neither concept maps to
the other chain family.

**account** is primarily Solana because Solana distinguishes between signing
keypairs (wallets) and non-signing accounts (PDAs, token accounts, data accounts).
EVM does not distinguish at the protocol level, so `account` is "limited" on EVM
with a warning to use `wallet` or `contract` instead.

**topic_filter** is EVM-only because `eth_getLogs` natively supports topic-based
filtering. Solana logs are program-level text, not indexed topics.

**market** is Hyperliquid-only because HyperCore has a native market concept with
dedicated REST/WS endpoints. On-chain DEX markets (Solana, EVM) use `pool` or
`contract`.

**pool** works on EVM (log-based from pool contract) and Solana (account-key-based
from pool account) but not on Hyperliquid (HyperCore pools are not on-chain; use
`market`).

**protocol** works on EVM and Solana where protocols span multiple contracts or
programs. Not applicable on Hyperliquid where HyperCore itself is the protocol.

---

## 4. Mode Semantics

Each target has a `mode` field: `backfill`, `stream`, or `both`.

### 4.1 backfill

Connector calls `backfill(target, checkpoint, limit)` to fetch historical data.
No streaming subscription is opened. Use cases: one-time historical import,
loading past events for a newly tracked contract.

### 4.2 stream

Connector calls `stream(target, checkpoint)` to open a real-time subscription.
No historical backfill is performed. Use cases: real-time monitoring, live market
data feed.

### 4.3 both

Connector calls `backfill` first to catch up, then opens a `stream` subscription.
Most common mode. Use cases: full wallet history with ongoing monitoring, complete
contract event history with live updates.

### 4.4 Mode availability per connector

| Connector | backfill | stream |
|---|---|---|
| `SolanaRpcConnector` | Yes | No |
| `SolanaGrpcConnector` | Limited | Yes |
| `EvmConnector` | Yes | Future |
| `HyperliquidRestConnector` | Yes | No |
| `HyperliquidWsConnector` | No | Yes |

When a target has `mode = both` and a single connector only supports one mode,
the control plane coordinates between connectors for the same chain family. For
example, `mode = both` on a Solana wallet uses `SolanaRpcConnector` for backfill
and `SolanaGrpcConnector` for streaming.

---

## 5. filter_spec Shape Summary

### 5.1 When filter_spec is required vs optional

| Target Kind | Required? | Reason |
|---|---|---|
| wallet | Optional | `address` fully identifies the target |
| contract | Optional | `address` fully identifies the target |
| program | Optional | `address` fully identifies the target |
| account | Optional | `address` fully identifies the target |
| topic_filter | **Required** | No `address`; defined by topic criteria |
| market | Optional | `address` (symbol) identifies the target |
| pool | Optional | `address` fully identifies the target |
| protocol | **Required** | No single `address`; defined by address set |

### 5.2 Validation rules

1. If `filter_spec` is required and null/empty, reject the registration.
2. Validate shape against the expected schema for the target kind.
3. Unknown keys are ignored (forward compatibility) but logged as warnings.
4. Values must use the correct format for the chain family (hex for EVM, base58
   for Solana).

### 5.3 filter_spec JSON schemas per target kind

**wallet:**
```json
{
  "type": "object",
  "properties": {
    "asset_filter": { "type": "array", "items": { "type": "string" } },
    "min_amount": { "type": "string" },
    "directions": { "type": "array", "items": { "enum": ["inbound", "outbound", "self"] } }
  }
}
```

**contract:**
```json
{
  "type": "object",
  "properties": {
    "event_signatures": { "type": "array", "items": { "type": "string" } },
    "topic_filters": { "type": "object" }
  }
}
```

**program:**
```json
{
  "type": "object",
  "properties": {
    "instruction_discriminators": { "type": "array", "items": { "type": "string" } },
    "include_cpi": { "type": "boolean" }
  }
}
```

**account:**
```json
{
  "type": "object",
  "properties": {
    "access_types": { "type": "array", "items": { "enum": ["write", "read"] } },
    "associated_program": { "type": "string" }
  }
}
```

**topic_filter:**
```json
{
  "type": "object",
  "required": ["topics"],
  "properties": {
    "topics": {
      "type": "array",
      "maxItems": 4,
      "items": {
        "oneOf": [
          { "type": "string" },
          { "type": "null" },
          { "type": "array", "items": { "type": "string" } }
        ]
      }
    },
    "address_filter": { "type": "array", "items": { "type": "string" } }
  }
}
```

**market:**
```json
{
  "type": "object",
  "properties": {
    "data_types": { "type": "array", "items": { "enum": ["fills", "funding", "liquidations"] } },
    "min_size": { "type": "string" }
  }
}
```

**pool:**
```json
{
  "type": "object",
  "properties": {
    "protocol": { "type": "string" },
    "token_pair": { "type": "array", "items": { "type": "string" }, "maxItems": 2 },
    "event_types": { "type": "array", "items": { "type": "string" } }
  }
}
```

**protocol:**
```json
{
  "type": "object",
  "required": ["name", "addresses"],
  "properties": {
    "name": { "type": "string" },
    "addresses": {
      "type": "object",
      "additionalProperties": {
        "type": "array",
        "items": {
          "type": "object",
          "required": ["address", "role"],
          "properties": {
            "address": { "type": "string" },
            "role": { "type": "string" },
            "label": { "type": "string" }
          }
        }
      }
    }
  }
}
```

---

## 6. Target Identity and Uniqueness Rules

### 6.1 Simple targets (address-identified)

For `wallet`, `contract`, `program`, `account`, `market`, and `pool`:

```
UNIQUE (kind, network, address)
```

If a target with the same `(kind, network, address)` exists, the system returns
the existing target. The `filter_spec`, `mode`, `label`, and `owner_id` can be
updated on the existing target.

### 6.2 Complex targets (filter-identified)

For `topic_filter` and `protocol`:

```
UNIQUE (kind, network, filter_spec_hash)
```

`filter_spec_hash` is a deterministic hash of the canonicalized `filter_spec`
JSONB, stored as an additional column on `index_targets`.

### 6.3 Network scoping

Every target is scoped to a single `network` value. Protocol targets that span
networks use a per-network `addresses` map in `filter_spec`; the `network` field
on the row indicates the primary network. The control plane creates per-network
ingestion runs.

### 6.4 Address normalization

Addresses are normalized before storage and uniqueness comparison:

- **EVM:** lowercased hex with `0x` prefix. Display may use checksummed format,
  but storage and comparison use lowercase.
- **Solana:** base58 as-is (case-sensitive by definition).
- **Hyperliquid:** same as EVM (lowercased hex with `0x` prefix).

---

## 7. Ownership and Tenancy Rules

### 7.1 owner_id semantics

The `owner_id` field is an optional UUID associating a target with a tenant or
user. It is **not** the same as the current `Transaction.user_id`
(`core/src/models.rs` line 25):

- `owner_id` answers "who registered this indexing target?" (control-plane concept)
- `Transaction.user_id` answers "who owns this raw data?" (V2 removes this from raw data per RFC Section 3.1, rule 1)

### 7.2 Multi-tenancy model

- **Shared target (`owner_id = NULL`):** visible to all API consumers. Default for
  system-registered targets.
- **Tenant-scoped target (`owner_id = <uuid>`):** associated with a specific owner.
  Access control applied at the API layer.

### 7.3 Shared raw data

Even when targets are tenant-scoped, raw data is canonical and deduplicated. Two
tenants tracking the same wallet address on the same network share the same
`raw_transactions` rows via separate `target_matches` entries. Only the
`index_targets` rows differ in `owner_id`.

This preserves the P0-W1 RFC principle (Section 3.1, rule 3): one row per on-chain
fact, regardless of how many targets reference it.

### 7.4 Ownership transfer

Targets can be transferred between owners by updating `owner_id`. This does not
affect the underlying raw data or target matches.

---

## 8. Example Specs (One Per Target Kind)

### 8.1 Solana wallet

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440001",
  "kind": "wallet",
  "network": "solana-mainnet",
  "chain_family": "solana",
  "address": "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
  "filter_spec": null,
  "mode": "both",
  "label": "My Solana Wallet",
  "owner_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

Backfill via `getSignaturesForAddress`. Stream via Yellowstone gRPC `account_include`.

### 8.2 EVM contract (USDC on Ethereum)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440002",
  "kind": "contract",
  "network": "ethereum-mainnet",
  "chain_family": "evm",
  "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
  "filter_spec": { "event_signatures": ["Transfer(address,address,uint256)"] },
  "mode": "backfill",
  "label": "USDC Transfer Events",
  "owner_id": null
}
```

`eth_getLogs` with contract in `address` filter and `Transfer` topic0 hash.

### 8.3 Solana program (SPL Token Program)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440003",
  "kind": "program",
  "network": "solana-mainnet",
  "chain_family": "solana",
  "address": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
  "filter_spec": { "instruction_discriminators": ["0x03"], "include_cpi": true },
  "mode": "stream",
  "label": "SPL Token Transfer Instructions",
  "owner_id": null
}
```

Yellowstone gRPC `account_include` with program ID. Discriminator filtering at
the target-matching layer.

### 8.4 Solana account (Marinade State PDA)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440004",
  "kind": "account",
  "network": "solana-mainnet",
  "chain_family": "solana",
  "address": "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC",
  "filter_spec": { "access_types": ["write"], "associated_program": "MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD" },
  "mode": "both",
  "label": "Marinade State Account",
  "owner_id": null
}
```

Backfill + stream via account pubkey. Only writable-set matches generate target
matches.

### 8.5 EVM topic_filter (USDC/USDT Transfers to treasury)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440005",
  "kind": "topic_filter",
  "network": "ethereum-mainnet",
  "chain_family": "evm",
  "address": null,
  "filter_spec": {
    "topics": [
      "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
      null,
      "0x000000000000000000000000742d35cc6634c0532925a3b844bc9e7595f2bd18"
    ],
    "address_filter": [
      "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
      "0xdac17f958d2ee523a2206206994597c13d831ec7"
    ]
  },
  "mode": "backfill",
  "label": "USDC/USDT Transfers to Treasury",
  "owner_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

`eth_getLogs` with `address = [USDC, USDT]`, `topics[0] = Transfer`, `topics[2] = padded treasury`.

### 8.6 Hyperliquid market (ETH-PERP)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440006",
  "kind": "market",
  "network": "hypercore-mainnet",
  "chain_family": "hyperliquid",
  "address": "ETH",
  "filter_spec": { "data_types": ["fills", "funding"] },
  "mode": "both",
  "label": "ETH-PERP Market",
  "owner_id": null
}
```

REST backfill for fills/funding. WebSocket market trade and funding feeds for streaming.

### 8.7 EVM pool (Uniswap V3 USDC/WETH)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440007",
  "kind": "pool",
  "network": "ethereum-mainnet",
  "chain_family": "evm",
  "address": "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640",
  "filter_spec": { "protocol": "uniswap_v3", "token_pair": ["USDC", "WETH"], "event_types": ["Swap", "Mint", "Burn"] },
  "mode": "backfill",
  "label": "Uniswap V3 USDC/WETH 0.05%",
  "owner_id": null
}
```

`eth_getLogs` with pool address. Event-type narrowing at the match layer.

### 8.8 EVM protocol (Aave V3 on Ethereum)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440008",
  "kind": "protocol",
  "network": "ethereum-mainnet",
  "chain_family": "evm",
  "address": null,
  "filter_spec": {
    "name": "aave_v3",
    "addresses": {
      "ethereum-mainnet": [
        { "address": "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2", "role": "pool", "label": "Aave V3 Pool" },
        { "address": "0x2f39d218133afab8f2b819b1066c7e434ad94e9e", "role": "registry", "label": "Pool Addresses Provider" }
      ]
    }
  },
  "mode": "backfill",
  "label": "Aave V3 (Ethereum)",
  "owner_id": null
}
```

Control plane expands into per-address `eth_getLogs` queries. All matching raw
transactions link via `target_matches` with address-specific match reasons.

### 8.9 Hyperliquid wallet (user)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440009",
  "kind": "wallet",
  "network": "hypercore-mainnet",
  "chain_family": "hyperliquid",
  "address": "0x742d35cc6634c0532925a3b844bc9e7595f2bd18",
  "filter_spec": null,
  "mode": "both",
  "label": "HL Trading Account",
  "owner_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

REST backfill: `fetch_user_fills`, `fetch_user_funding`, `fetch_user_ledger_updates`.
WebSocket: user event subscription.

### 8.10 EVM wallet

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440010",
  "kind": "wallet",
  "network": "ethereum-mainnet",
  "chain_family": "evm",
  "address": "0x742d35cc6634c0532925a3b844bc9e7595f2bd18",
  "filter_spec": { "asset_filter": ["USDC", "ETH"] },
  "mode": "backfill",
  "label": "Treasury EOA",
  "owner_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

ERC-20 Transfer logs where `from` or `to` match this address. Native ETH transfer
scanning. Does **not** use `eth_getLogs` with this address in the `address` filter
because EOAs do not emit logs (P0-W1 RFC Section 6.1).

---

## 9. Wallet-vs-Account and Wallet-vs-Contract Distinctions

### 9.1 Wallet vs account on Solana

On Solana, wallets and accounts are fundamentally different:

- **Wallet:** a keypair with signing authority. Match reasons: "signer",
  "fee_payer", "sender", "receiver".
- **Account:** any pubkey (PDAs, token accounts, data accounts, vaults). Match
  reasons: "account_key", "writable_account", "readable_account".

The ingestion primitive is the same (`getSignaturesForAddress` or gRPC
`account_include`), but downstream interpretation differs.

### 9.2 Wallet vs contract on EVM

Both are 20-byte addresses, but:

- **Wallet (EOA):** controlled by a private key. Requires Transfer log scanning
  and native transaction scanning.
- **Contract:** has code at its address and emits logs. Uses `eth_getLogs` with
  address as log emitter.

Using `eth_getLogs` with a wallet address in the `address` filter returns nothing
useful because EOAs do not emit logs. This is the semantic error documented in
P0-W1 RFC Section 6.1 that V2 fixes.

---

## 10. Cross-References

### 10.1 P0-W1 (V2 Architecture RFC) dependencies

This spec answers the two questions P0-W1 explicitly deferred to P0-W2:

| Deferred Question (P0-W1 Section 14) | Answer Location |
|---|---|
| Exact `filter_spec` JSONB shape per target kind | Section 5.3 |
| Per-chain-family validity matrix for target kinds | Section 3 |

Additionally, this spec is consistent with:
- P0-W1 Section 3.2: `index_targets` table shape (reproduced in Section 1)
- P0-W1 Section 4.2: V2 Connector trait accepts `IndexTarget` (Section 4 mode mapping)
- P0-W1 Section 3.1 rule 1: no consumer identity in Bronze (Section 7.1)
- P0-W1 Section 3.1 rule 3: one row per on-chain fact (Section 7.3)
- P0-W1 Section 6.1: EVM `eth_getLogs` address semantics (Section 9.2)
- P0-W1 Section 6.2: Solana gRPC target mismatch (Sections 2.3, 2.4)
- P0-W1 Section 6.3: Hyperliquid user vs market (Sections 2.1, 2.6)

### 10.2 P0-W3 (Network Model) dependencies

This spec uses network IDs defined by P0-W3:
- `solana-mainnet`, `ethereum-mainnet`, `hypercore-mainnet` (appear in example
  specs, Section 8)
- `base-mainnet`, `hyperevm-mainnet` (referenced in target kind definitions,
  Sections 1 and 2.2)
- `chain_family_enum` values: `solana`, `evm`, `hyperliquid` (Section 1)

The P0-W3 handoff (`docs/phase0/p0-w3-network-model-handoff.md`) Section 8.1
confirms alignment with P0-W1 RFC Section 3.3. This spec's network references
are consistent with the frozen P0-W3 network registry.

---

## 11. Downstream Packet Mapping

| Downstream Packet | What This Spec Locks For It |
|---|---|
| **P0-W4** (Rollout Plan) | Target registration compatibility strategy; which target kinds need immediate support vs deferred |
| **P1-W1** (Core Types) | Rust `TargetKind` enum values, `IndexTarget` struct fields, `TargetMode` enum values |
| **P1-W2** (Migrations) | `target_kind_enum` SQL type, `target_mode_enum` SQL type, `index_targets` uniqueness constraints, `filter_spec_hash` column for complex targets |
| **P1-W3** (Repo Layer) | Target registration and lookup queries, `filter_spec` validation logic, address normalization |
| **P2-W1** (Connector Interface) | Which target kinds each connector must accept, mode coordination rules |
| **P2-W2** (Target Registration API) | Target registration request validation, filter_spec schema enforcement |
| **P2-W3** (EVM Fix) | Contract vs wallet vs topic_filter ingestion strategies; EVM validity column of matrix |
| **P2-W4** (Solana Fix) | Program vs account vs wallet target semantics; Solana validity column of matrix |
| **P2-W5** (Hyperliquid Targets) | Market vs wallet target semantics; Hyperliquid validity column of matrix |

---

## 12. Decisions Locked By This Spec

| # | Decision | Choice | Rationale |
|---|---|---|---|
| 1 | Eight target kinds | wallet, contract, program, account, topic_filter, market, pool, protocol | Covers all target types from RFC Section 3.2 |
| 2 | `account` distinct from `wallet` | Separate kind with distinct match semantics | Solana PDAs are real indexing subjects that are not wallets |
| 3 | `filter_spec` is JSONB with per-kind schemas | Typed validation at app layer, flexible JSONB in PostgreSQL | Supports chain-specific filtering without schema migration for every new filter option |
| 4 | Simple targets use `(kind, network, address)` uniqueness | Prevents duplicate ingestion | Deduplicates work while allowing different owners |
| 5 | Complex targets use `filter_spec_hash` for uniqueness | Deterministic hash of canonicalized JSONB | Prevents duplicate topic filters and protocol targets |
| 6 | `owner_id` is optional and control-plane-only | Not baked into raw data | Preserves RFC principle of target-agnostic Bronze |
| 7 | Addresses normalized before storage | Lowercase hex for EVM/HL, base58 as-is for Solana | Prevents duplicate targets due to case differences |
| 8 | `mode` controls connector invocation | backfill/stream/both with cross-connector coordination | Maps to the two-method Connector trait from RFC Section 4.2 |

The following are key semantic clarifications drawn from the spec body text
(Sections 2.1 and 2.8) that downstream packets should treat as binding:

| # | Clarification | Detail | Source |
|---|---|---|---|
| 9 | Wallet filter_spec narrows queries, not ingestion | Connectors fetch all wallet activity; filtering at match/query layer | Section 2.1 |
| 10 | Protocol targets expand to per-address sub-queries | Control plane handles expansion as an implementation detail | Section 2.8 |

---

## 13. Verification Notes

This handoff was verified against the codebase at commit `7f72cd9` (tip of `main`
as of 2026-03-08). No runtime code was changed by this work packet.

All target kinds, filter_spec schemas, and ingestion strategies are consistent
with:
- P0-W1 handoff (`docs/phase0/p0-w1-v2-architecture-rfc-handoff.md`)
- P0-W3 handoff (`docs/phase0/p0-w3-network-model-handoff.md`)
- Current codebase wallet-centric assumptions documented in P0-W1 Sections 9.1-9.5

The validity matrix covers all three chain families and all eight target kinds.
Example specs cover all target kinds with at least one complete example each
(10 examples total).

Verification commands run:
- `cargo fmt --all --check` -- passed
- `cargo clippy --workspace --all-targets -- -D warnings` -- passed
- `cargo test --workspace` -- passed
