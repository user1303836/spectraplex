# V2 Authority And ETL Implementation Plan

Status: **Active, updated after merged PRs through #272; P0-P3 substantially landed, P4/P5 remain**
Date: 2026-04-25
Codebase: `6ad3a5a` (`6ad3a5a7cb3a6d6229d012d883a886407f1972da`)
Previous snapshot: 2026-04-23 / `bd56127`
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
- #268: honest supported-path smoke and README alignment
- #269: target-scoped Gold lineage and target-centric ingest groundwork
- #270: tenant status and export provenance usability
- #271: fail loudly when smoke prerequisites are missing
- #272: target-centric connector ingestion for Hyperliquid market targets

Most of the original authority/ETL architecture work is now merged. The active
plan is no longer "design V2"; it is to make the claimed supported path true,
limit the MVP to what is actually supportable, and close the gaps that still
prevent Spectraplex from being an honest general-purpose indexer MVP.

## 1. Current Conclusion

The repo is now close to an honest API MVP with general-purpose V2 building
blocks. Recent PRs closed the biggest previously identified gaps: the smoke
path fails loudly, target-scoped Gold query/export no longer relies on the
invalid generic `raw_transaction_id` join for Gold tables, tenant users can see
owned completeness/status, export provenance is useful, and the first
non-wallet target-centric ingest path exists for Hyperliquid market targets.

What is already true:

- V2 Bronze, target registry, network/provider registry, durable jobs,
  streaming subscriptions, materialization runs, and export jobs exist.
- Bronze-driven Silver and Gold materialization exists for the planned dataset
  set.
- Tenant-scoped auth and owner checks exist at the API layer.

What is not yet true enough to call the general-purpose MVP ready:

- This PR publishes the first stable/beta/experimental/unsupported support
  matrix in the README. The remaining work is to keep that matrix backed by
  representative fixtures and bug triage as the supported surface evolves.
- Correctness fixtures are still thin for the support matrix. The code has
  focused unit/integration coverage for target scoping, status/provenance, and
  runtime plumbing, but not representative fixture coverage for every path the
  docs should label stable or beta.
- EVM contract/topic target-centric ingest is intentionally advertised as
  unsupported. PR #272 kept it disabled rather than shipping a misleading
  half-working runtime path.
- Several high/critical operational correctness issues remain open in GitHub
  (#248-#267). They are not all MVP blockers, but the support matrix should make
  their impact visible instead of implying production-grade coverage.

The next phase should therefore focus on making the support boundaries explicit
and backing stable claims with representative fixtures, not on another broad
architecture rewrite.

## 2. Workstream Status

| Workstream | Current status | Notes |
|---|---|---|
| A. Canonical registries and type cleanup | **Done for MVP** | `DatasetRegistry` is the canonical dataset registry. `DatasetName` covers Silver and Gold public names and physical table mapping. Future work is maintenance only. |
| B. First-class network and provider configuration | **Done for MVP** | Structured provider config, `ProviderRegistry`, `NetworkContext`, and explicit `network` handling are in place. Legacy singleton config remains as compatibility. |
| C. Durable control plane | **Done for MVP** | Ingestion jobs, export jobs, stream subscriptions, materialization runs, leases, heartbeat, reclaim, and restart-oriented worker loops are DB-backed. |
| D. V2-authoritative ingestion and streaming | **Partial, improved by #272** | Wallet ingest remains first-class and some runtime code still uses V1-shaped adapter output, but `POST /v1/targets/:id/ingest` now supports Hyperliquid market targets through the connector path. |
| E. Bronze -> Silver -> Gold pipeline | **Done for MVP scope, needs fixtures** | Dataset coverage exists and Gold tables that lack `raw_transaction_id` use direct target filters. Remaining work is representative correctness validation for support-matrix claims. |
| F. Compatibility cutover and V1 de-emphasis | **Done for MVP docs, partial architecturally** | `/v1/normalize` requires `ingestion_run_id`, wallet reads are V2-backed, compat writes are gated, and README labels CLI normalize as compatibility. Some V1 projection remains intentionally available. |
| G. Integrator UX improvements | **Partial** | Per-tenant API keys, owner-scoped handlers, smoke-path hardening, status/completeness, export provenance, target-centric ingest docs, and an initial support matrix exist. Remaining work is keeping support labels tied to fixtures and known operational risks. |
| H. Verification and operational hardening | **Partial** | Restart/reclaim, smoke prerequisite checks, tenant status, and Gold lineage tests improved materially. Remaining work is matrix-driven correctness fixtures and triage of open high/critical operational bugs. |

## 3. Milestone Status

| Milestone | Status | Update |
|---|---|---|
| 1. Canonical naming and provider model | **Done** | No longer a planning blocker. |
| 2. Durable control plane runtime | **Done** | Durable ingestion/export/stream/materialization state is in Postgres. |
| 3. V2-authoritative backfill ingest | **Partial, improved by #272** | V2 writes are authoritative. The public API now has a target-centric non-wallet path for Hyperliquid market targets, but EVM contract/topic targets remain intentionally unsupported at runtime. |
| 4. V2-authoritative streams | **Partial** | Durable streams exist, but the stable supported stream story is still narrow and not yet a broad target-driven runtime surface. |
| 5. Bronze-driven materialization pipeline | **Done for MVP scope, needs fixtures** | Coverage and target lineage are present for supported dataset routes; correctness fixtures should now define exactly which Gold semantics are stable. |
| 6. V2-backed reads and compatibility cutover | **Partial** | V2-backed reads and no-run normalize removal are done. V1 compatibility writes and CLI V1 paths remain. |
| 7. Integrator polish and operational hardening | **Partial** | The documented happy path is much closer to true, and this PR publishes explicit support levels. The next gap is backing stable labels with representative fixtures and provider-hardening triage. |

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
- Gold tables do not all carry `owner_id` directly. Isolation is enforced via
  target ownership checks and dataset-specific target-scoped queries, not
  database row-level ownership on every Gold row.
- The previous invalid generic `raw_transaction_id` assumption for Gold
  query/export was fixed by #269, but the direct-filter paths should stay under
  regression coverage as support labels evolve.

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

- Keep target-scoped Gold query/export lineage covered by regression tests. The
  invalid generic `raw_transaction_id` assumption was fixed by #269 for Gold
  tables that require direct target filtering.
- Keep Gold `dataset_completeness` and export provenance covered as supported
  labels evolve.
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
needs a small honest matrix, currently:

- Solana wallet as the stable API path,
- EVM wallet and Hyperliquid wallet as beta wallet paths,
- Hyperliquid market as a beta Bronze-only target-centric path,
- EVM contract/topic_filter explicitly unsupported until connector/runtime
  hardening lands.

Wallet-derived Gold datasets remain first-class for the first usable release.
For non-wallet targets, the MVP bar should follow the support matrix: do not
claim Bronze/Silver query/export until the runtime path and fixtures prove it.

### P0: Make The Supported Path Honest And Provable

Status: **Substantially landed by #268 and #271**.

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

Status: **Substantially landed by #269**.

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

Status: **First supported non-wallet slice landed by #272**. Hyperliquid market
targets can ingest through `POST /v1/targets/:id/ingest`; EVM contract/topic
targets remain intentionally unsupported at runtime until their connector path
is complete enough to advertise honestly.

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

Status: **Substantially landed by #270**.

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

Status: **Initial matrix published by this PR; correctness fixtures remain active next work**.

Keep the MVP boundaries explicit and verified:

- maintain the support matrix labels for target kinds and dataset flows as
  `stable`, `beta`, `experimental`, or unsupported,
- add representative correctness fixtures for the supported matrix,
- validate Gold semantics that downstream consumers are likely to rely on
  (`balance_history`, HL PnL/trade grouping, `pool_snapshots`).

Acceptance criteria:

- operators can tell which paths are actually supported,
- tests cover the supported target matrix instead of only internal helpers.

### P5: Keep The Compatibility Story Explicit

Status: **Partially landed; keep maintaining as the matrix evolves**.

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

With P0-P3 substantially landed, the best next slices are:

1. **Correctness fixture scaffold**
   - Add representative fixture helpers for the stable/beta matrix, starting
     with deterministic Gold semantics that do not require live providers.
   - Prioritize Solana wallet ledger/balance history and Hyperliquid market
     Silver ingestion semantics before broader protocol TVL claims.

2. **Triage critical provider correctness issues**
   - Use the support matrix to decide which open issues (#248-#267) are MVP
     blockers versus beta/experimental caveats.
   - Fix critical stable-path bugs before labeling any path production-grade.

3. **Expand target-centric connector coverage**
   - Add EVM contract/topic target-centric ingest only when chain-id/finality,
     range shrinking, and fixture coverage are good enough for the advertised
     support level.

4. **Maintain support matrix alignment**
   - Update README labels whenever a bug fix or fixture changes what can be
     advertised honestly.

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
