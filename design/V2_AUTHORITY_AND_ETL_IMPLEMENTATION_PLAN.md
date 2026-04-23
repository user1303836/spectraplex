# V2 Authority And ETL Implementation Plan

Status: **Active, updated after merged PRs through #247; re-scoped to honest MVP readiness**
Date: 2026-04-23
Codebase: `bd56127` (`bd56127f5e6b7e34a643d28420bf1f38e7db8b7a`)
Previous snapshot: 2026-04-22 / `c52625d`
Audience: follow-on implementation agent(s)

This document supersedes the 2026-04-22 version at
`design/V2_AUTHORITY_AND_ETL_IMPLEMENTATION_PLAN.md`. Since that snapshot, the
following PRs landed:

- #240: remove legacy V1 normalize fallback
- #241: complete Gold materialization coverage
- #242: add HMAC signing to callbacks
- #243: replace String fields with typed enums on `IngestionRun` and `ExportJob`
- #244: per-user/tenant isolation at the query layer
- #245: prove the API happy path
- #246: make status and provenance usable
- #247: freeze compatibility story, operational hardening, and integrator UX

Most of the original authority/ETL architecture work is now merged. The active
plan is no longer "design V2"; it is to make the claimed supported path true,
limit the MVP to what is actually supportable, and close the gaps that still
prevent Spectraplex from being an honest general-purpose indexer MVP.

## 1. Current Conclusion

The repo is now close to a wallet-first API MVP with general-purpose V2
building blocks, but it is not yet an honest "general-purpose blockchain
indexer MVP" in the sense the README and recent plan snapshots imply.

What is already true:

- V2 Bronze, target registry, network/provider registry, durable jobs,
  streaming subscriptions, materialization runs, and export jobs exist.
- Bronze-driven Silver and Gold materialization exists for the planned dataset
  set.
- Tenant-scoped auth and owner checks exist at the API layer.

What is not yet true enough to call the general-purpose MVP ready:

- The public ingest/runtime path is still wallet-shaped. The API and worker
  runtime still rely on the legacy wallet-oriented `ChainIngestor` flow and
  only convert to V2 records after fetch. The general `Connector` abstraction
  exists but is not yet the supported runtime contract.
- Generic target-scoped dataset query/export is structurally fragile for some
  Gold datasets. The shared dataset filter builder assumes the queried table
  has `raw_transaction_id`, but several Gold tables do not, so target-scoped
  Gold query/export must be treated as suspect until fixed.
- The checked-in smoke test does not currently prove the supported path. It
  uses stale request/response shapes, omits required tenant `target_id` on
  dataset/export calls, and tolerates failures while still printing success.
- Tenant users still cannot inspect dataset completeness/status for their own
  targets.
- The docs currently overstate readiness relative to the code's actual
  supported path.

The next phase should therefore focus on making the advertised path true, not
on another broad architecture rewrite.

## 2. Workstream Status

| Workstream | Current status | Notes |
|---|---|---|
| A. Canonical registries and type cleanup | **Done for MVP** | `DatasetRegistry` is the canonical dataset registry. `DatasetName` covers Silver and Gold public names and physical table mapping. Future work is maintenance only. |
| B. First-class network and provider configuration | **Done for MVP** | Structured provider config, `ProviderRegistry`, `NetworkContext`, and explicit `network` handling are in place. Legacy singleton config remains as compatibility. |
| C. Durable control plane | **Done for MVP** | Ingestion jobs, export jobs, stream subscriptions, materialization runs, leases, heartbeat, reclaim, and restart-oriented worker loops are DB-backed. |
| D. V2-authoritative ingestion and streaming | **Partial** | Runtime writes V2 Bronze/checkpoints/runs first, but the supported ingest path is still wallet-shaped and still executes through the legacy wallet-oriented adapter flow rather than the general `Connector` contract. |
| E. Bronze -> Silver -> Gold pipeline | **Partial for MVP** | Dataset coverage exists, but target-scoped query/export lineage for some Gold tables is not yet trustworthy because the shared query path assumes `raw_transaction_id` exists on the queried table. |
| F. Compatibility cutover and V1 de-emphasis | **Partial** | `/v1/normalize` requires `ingestion_run_id`, wallet reads are V2-backed, and compat writes are gated. But V1-shaped adapter flow and compat projection are still part of the supported runtime path. |
| G. Integrator UX improvements | **Partial** | Per-tenant API keys and owner-scoped handlers landed, but tenant-scoped dataset completeness/status is still unavailable and the README/smoke path currently overstate readiness. |
| H. Verification and operational hardening | **Partial** | Restart/reclaim and lease hardening improved materially, but the checked-in smoke path is not yet an honest supported-path proof and target-scoped Gold query/export still needs direct verification. |

## 3. Milestone Status

| Milestone | Status | Update |
|---|---|---|
| 1. Canonical naming and provider model | **Done** | No longer a planning blocker. |
| 2. Durable control plane runtime | **Done** | Durable ingestion/export/stream/materialization state is in Postgres. |
| 3. V2-authoritative backfill ingest | **Partial** | V2 writes are authoritative, but the supported public ingest path is still wallet-centric and not yet target-centric across the intended target model. |
| 4. V2-authoritative streams | **Partial** | Durable streams exist, but the stable supported stream story is still narrow and not yet a broad target-driven runtime surface. |
| 5. Bronze-driven materialization pipeline | **Partial for MVP** | Coverage is present, but correctness, target lineage, and export/query trustworthiness still need cleanup before Gold can be treated as broadly usable. |
| 6. V2-backed reads and compatibility cutover | **Partial** | V2-backed reads and no-run normalize removal are done. V1 compatibility writes and CLI V1 paths remain. |
| 7. Integrator polish and operational hardening | **Partial** | Recent PRs moved this forward materially, but the documented happy path still needs to become a real, trustworthy, end-to-end operator workflow. |

## 4. Previously Open Medium-Severity Issues

### #209: Callback HMAC Signing

Status: **Resolved by PR #242**.

Evidence in current code:

- `core/src/callback.rs` defines `sign_callback_payload()`.
- `core/src/config.rs` adds `callback_hmac_secret`.
- `api/src/main.rs::fire_callback()` and `api/src/worker.rs::fire_callback_best_effort()`
  add `X-Spectraplex-Signature: sha256=<hex>` when the secret is configured.
- `spectraplex.toml.example` documents `callback_hmac_secret`.

Remaining non-MVP follow-up: callback delivery attempt audit tables are still
optional polish, not required to resolve #209.

### #214: Typed Status Enums On `IngestionRun` And `ExportJob`

Status: **Resolved by PR #243**.

Evidence in current code:

- `core/src/v2.rs::IngestionRun` uses `IngestionJobMode` and
  `IngestionJobStatus`, not raw `String` fields.
- `core/src/v2.rs::ExportJob` uses `DatasetName`, `ExportFormat`, and
  `ExportJobStatus`.
- `adapters/src/v2_repo.rs` parses SQL strings into typed enums and fails on
  invalid values.

### #216: Per-User/Tenant Isolation At The Query Layer

Status: **Resolved for the query layer by PR #244, with explicit caveats**.

Evidence in current code:

- `migrations/20260422000001_add_api_keys_and_owner_scoping.sql` adds
  `api_keys`, `index_targets.owner_id` indexing, owner-aware target uniqueness,
  and `export_jobs.owner_id`.
- Auth middleware validates DB-backed API keys and injects
  `AuthenticatedOwner(Some(owner_id))`; the legacy config API key remains
  ownerless admin mode.
- Wallet routes call `check_wallet_owner()` and pass `owner.0` into V2 repo
  methods.
- Dataset record queries, exports, tax export, forensics, analytics, streams,
  target reads, job status, and export downloads validate `target_id` ownership
  for tenant-scoped requests.

Caveats that should stay visible:

- The legacy config-level API key is still admin/ownerless and bypasses tenant
  scoping by design.
- `/v1/datasets/:name/completeness` and `/v1/datasets/:name/status` now return
  owner-filtered completeness rows for tenant-scoped requests, but dataset
  version metadata remains global by design.
- Some Gold tables do not carry `owner_id` directly. Isolation is enforced via
  target ownership checks and target-scoped queries, not database row-level
  ownership on every Gold row.
- The shared Gold dataset query/export path still needs explicit validation and
  likely schema/query cleanup. The current generic filter builder assumes the
  queried table exposes `raw_transaction_id`, which is not universally true
  across Gold tables.

## 5. Gold Materialization Coverage

The 2026-04-21 doc is stale here. It said `wallet_ledger` was landed,
`balance_history` was skipped pending seeding, and the other Gold datasets were
still computed on demand. Current code has durable materialization coverage for
all six planned Gold datasets.

| Dataset | Current state |
|---|---|
| `wallet_ledger` | Materialized from Silver in `adapters/src/dual_write.rs::materialize_gold_from_silver()` and persisted via `save_wallet_ledger_records()`. |
| `balance_history` | Now materialized. Running balances are seeded from DB via `get_latest_balance_snapshots()` before applying incremental events. |
| `hl_pnl_summary` | Now materialized from Silver fills/funding via `compute_pnl_summary()` and persisted with `save_hl_pnl_summary()`. |
| `hl_trade_history` | Now materialized from Silver fills via `build_trade_history()` and persisted with `save_hl_trade_history()`. |
| `protocol_events` | Now materialized from Silver decoded events via `compute_protocol_events()` and persisted with `save_protocol_events()`. |
| `pool_snapshots` | Now materialized via `compute_pool_snapshots()` and persisted with `save_pool_snapshots()`. Current semantics are basic and should be validated before treating protocol TVL as production-grade. |

Registry/query/export status:

- `DatasetRegistry::gold_materializable()` now includes all six Gold datasets.
- `QUERYABLE_DATASETS` and `EXPORTABLE_DATASETS` include all six.
- Dataset record routes and streaming export support all six.
- HL/protocol analytics endpoints now read durable Gold tables through repo
  query/export methods and build response summaries from those records.

Remaining Gold work for MVP:

- Fix target-scoped Gold query/export lineage. The current generic dataset
  query builder assumes `dt.raw_transaction_id` exists on the queried table,
  but that is not true for every Gold dataset. Either add durable target/raw
  lineage to those tables or replace the shared query path with
  dataset-specific joins that are actually valid.
- Add or fix Gold `dataset_completeness` updates. Current Bronze-native
  completeness upserts are still Silver-oriented.
- Confirm export provenance for Gold datasets reports useful version and
  completeness metadata.
- Validate the semantics of `balance_history`, `hl_pnl_summary`, and
  `pool_snapshots` with representative data before calling the MVP usable for
  tax/analytics decisions.

## 6. V1 De-Emphasis State

### Done

- `/v1/normalize` requires `ingestion_run_id`.
- `api/src/materialize_worker.rs::execute_normalize()` fails closed without
  `ingestion_run_id`.
- The Bronze-native path materializes directly from `raw_transactions` and no
  longer reconstructs V1 `Transaction` values for API normalization.
- Wallet-facing API reads for transactions, single transaction lookup, ledger,
  balances, stats, and export are V2-backed behind compatibility response
  shapes.
- Gold analytics and dataset routes read durable V2/Gold tables.

### Still Present

- API ingestion and stream workers still depend on the legacy wallet-oriented
  adapter flow and convert V1-shaped adapter output with `v1_tx_to_v2_raw()`
  before V2 persistence.
- API workers still perform best-effort V1 compatibility writes using
  `save_transactions()` and V1 checkpoints.
- `adapters/src/dual_write.rs::materialize_silver_datasets()` still exists for
  legacy/manual paths.
- CLI direct ingest still uses V1-first dual-write helpers in some paths.
- CLI `normalize` still parses V1 transactions, writes `ledger_entries`, and
  calls `materialize_silver_datasets()`.
- V1 tables and parser-version names remain for compatibility and tests.

MVP decision: do not block shipping on deleting all V1 code. But also do not
pretend the runtime is fully target-centric yet. For the first usable release,
make the API workflow the supported path, label CLI normalize/legacy V1
projection as compatibility, and only de-emphasize the remaining V1 path after
the supported API workflow is actually proven and documented.

## 7. Active MVP Plan

### MVP Target

For this next phase, "general-purpose MVP" should mean:

1. a team member can configure Postgres and providers,
2. create or use a tenant-scoped API key,
3. register and ingest at least one supported target from a published support
   matrix,
4. observe job and materialization status,
5. query and export at least one Silver dataset and one Gold dataset scoped to
   that target,
6. repeat the flow after an API restart without losing jobs or operational
   truth.

This should not require every target kind to be production-grade. The MVP only
needs a small stable matrix, for example:

- Solana wallet
- Hyperliquid wallet or market
- EVM contract or topic_filter

Wallet-derived Gold datasets remain first-class for the first usable release.
For non-wallet targets, the MVP bar is successful Bronze/Silver ingestion plus
scoped query/export on the supported path.

### P0: Make The Supported Path Honest And Provable

Fix the checked-in smoke path and README so they reflect the real API:

- update the smoke script to use current request/response shapes,
- require it to fail on unexpected HTTP responses instead of swallowing them,
- include tenant `target_id` where the current tenant-scoped API requires it,
- prove ingest -> status -> materialize -> query -> export -> download using a
  deterministic path.

If a live provider-backed smoke test is too flaky, prefer a deterministic
fixture-backed path over a best-effort mainnet script that can print success on
broken requests.

Acceptance criteria:

- a fresh local environment can run the documented path successfully,
- failures are visible and actionable,
- the README no longer overclaims what the smoke path proves.

### P1: Fix Target-Scoped Gold Query And Export Lineage

Make target-scoped Gold reads actually trustworthy:

- audit every Gold dataset exposed through generic query/export,
- fix the shared filter path or replace it with dataset-specific query builders
  where the generic `raw_transaction_id` join assumption is invalid,
- add tests that prove tenant-scoped target queries and exports return only the
  requested target's rows for supported Gold datasets.

Acceptance criteria:

- target-scoped Gold query/export works correctly for all datasets labeled
  supported in the MVP matrix,
- no supported Gold endpoint depends on an invalid `raw_transaction_id` join.

### P2: Make Ingest Target-Centric, Not Just Wallet-Centric

Move the supported ingest/runtime path toward the actual target model:

- add a supported target-centric ingest entry point, such as
  `POST /v1/targets/:id/ingest` or an equivalent `target_id`-driven ingest
  request,
- route supported target kinds through the V2 `Connector` abstraction rather
  than only the legacy wallet-oriented `ChainIngestor` path,
- keep the wallet path first-class, but stop treating it as the only real
  supported ingestion surface.

Acceptance criteria:

- at least one non-wallet target type is supported end-to-end through the
  public API path,
- the supported path is described in terms of targets, not just wallets.

### P3: Make Tenant Status And Provenance Usable

Fix the status surfaces that real users will inspect:

- add tenant-scoped dataset completeness/status or add a target-specific status
  endpoint that tenants can safely use,
- ensure Gold `dataset_completeness` is updated when Gold records are written,
- ensure export provenance for supported Gold datasets reflects useful version
  and completeness information.

Acceptance criteria:

- tenant users can inspect their own target's materialization state without
  seeing global state,
- completed exports carry provenance a downstream integrator can actually use.

### P4: Publish A Real Support Matrix And Correctness Fixtures

Make the MVP boundaries explicit:

- publish a support matrix that labels target kinds and dataset flows as
  `stable`, `beta`, or `experimental`,
- add representative correctness fixtures for the supported matrix,
- validate Gold semantics that downstream consumers are likely to rely on
  (`balance_history`, HL PnL/trade grouping, `pool_snapshots`).

Acceptance criteria:

- operators can tell which paths are actually supported,
- tests cover the supported target matrix instead of only internal helpers.

### P5: Keep The Compatibility Story Explicit

Keep V1 compatibility bounded and understandable:

- document API ingestion + auto-materialization as the supported path,
- keep CLI `normalize` labeled as legacy compatibility unless it is moved onto
  the Bronze-native flow,
- retain the V1 compat-write flag and document when disabling it is safe.

Acceptance criteria:

- a user cannot accidentally choose a legacy path believing it is the
  recommended V2 flow,
- optional V1 projection is clearly described as compatibility, not authority.

## 8. Updated First PRs

The old suggested first PRs are no longer correct. The best next slices are:

1. **Honest supported-path smoke and README alignment**
   - Fix the smoke script so it uses the real API contract and fails loudly.
   - Align README curl examples with the current tenant-scoped API.

2. **Target-scoped Gold query/export lineage**
   - Fix or replace the generic Gold query/export path where `raw_transaction_id`
     is assumed but not actually present.
   - Add focused tests for supported Gold datasets.

3. **Target-centric ingest API/runtime**
   - Add a supported `target_id`-driven ingest path.
   - Move supported non-wallet target kinds onto the `Connector` path.

4. **Tenant status/completeness**
   - Add safe tenant-visible status surfaces for owned targets.
   - Wire Gold completeness/provenance through those surfaces.

5. **Support matrix and correctness fixtures**
   - Publish the MVP support matrix.
   - Add representative fixtures for Solana wallet, Hyperliquid wallet/market,
     and EVM contract/topic flows.

## 9. Definition Of Done For MVP

The MVP is ready when:

- the documented API path runs successfully on a fresh local environment
  without ignored failures,
- ingestion, materialization, query, and export survive an API restart,
- at least one non-wallet target type is supported end to end through the
  public API path,
- supported target-scoped Gold query/export paths are correct for the datasets
  labeled stable in the support matrix,
- wallet and dataset reads are tenant-scoped for DB-backed API keys,
- users can see enough status/provenance to trust a completed job/export,
- the support matrix is explicit and matches what the code actually proves,
- V1 compatibility behavior is documented and does not surprise operators.

Out of scope for MVP:

- deleting all V1 tables/code,
- production-grade support for every target kind on every chain,
- full SDK generation,
- dashboard/frontend work,
- enterprise org/billing management,
- exhaustive reorg/finality modeling.
