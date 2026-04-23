# V2 Authority And ETL Implementation Plan

Status: **Active, updated after merged PRs through #244; narrowed to MVP readiness**
Date: 2026-04-22
Codebase: `c52625d` (`c52625dbebc38eb03358cd0d74d7481175d3c1cf`)
Previous snapshot: 2026-04-21 / `4df29a8`
Audience: follow-on implementation agent(s)

This document supersedes the 2026-04-21 version at
`design/V2_AUTHORITY_AND_ETL_IMPLEMENTATION_PLAN.md`. Since that snapshot, the
following PRs landed:

- #240: remove legacy V1 normalize fallback
- #241: complete Gold materialization coverage
- #242: add HMAC signing to callbacks
- #243: replace String fields with typed enums on `IngestionRun` and `ExportJob`
- #244: per-user/tenant isolation at the query layer

The result is that most of the original authority/ETL architecture work is no
longer planning work. The active plan should now focus on proving a usable MVP
path, closing the remaining compatibility edges, and documenting the workflow.

## 1. Current Conclusion

V2 is now the authoritative API/runtime path for ingestion, streaming,
normalization, dataset query, and export. Bronze-driven materialization writes
Silver and all planned Gold datasets from V2 records. The old no-run API
normalize fallback is gone.

The remaining work is not another broad architecture pass. The minimum viable
path is:

1. prove one reliable operator workflow end to end,
2. make the remaining V1 compatibility surfaces explicit and non-blocking,
3. make tenant-scoped status/completeness and export provenance usable enough,
4. add the smallest possible docs/smoke tests so a team member can run it
   without reading internal Rust code.

## 2. Workstream Status

| Workstream | Current status | Notes |
|---|---|---|
| A. Canonical registries and type cleanup | **Done for MVP** | `DatasetRegistry` is the canonical dataset registry. `DatasetName` covers Silver and Gold public names and physical table mapping. Future work is maintenance only. |
| B. First-class network and provider configuration | **Done for MVP** | Structured provider config, `ProviderRegistry`, `NetworkContext`, and explicit `network` handling are in place. Legacy singleton config remains as compatibility. |
| C. Durable control plane | **Done for MVP** | Ingestion jobs, export jobs, stream subscriptions, materialization runs, leases, heartbeat, reclaim, and restart-oriented worker loops are DB-backed. |
| D. V2-authoritative ingestion and streaming | **Done for API MVP** | API backfill and stream flows write V2 Bronze/checkpoints/runs first and auto-enqueue materialization. Internal connectors still emit V1-shaped `Transaction` values before conversion. |
| E. Bronze -> Silver -> Gold pipeline | **Done for planned dataset coverage; hardening remains** | Bronze-native normalize writes Silver and all planned Gold datasets: `wallet_ledger`, `balance_history`, `hl_pnl_summary`, `hl_trade_history`, `protocol_events`, `pool_snapshots`. Gold completeness/provenance still needs MVP-level cleanup. |
| F. Compatibility cutover and V1 de-emphasis | **Done for MVP** | `/v1/normalize` requires `ingestion_run_id` and the worker fails closed without it. Wallet API reads are V2-backed. V1 compat writes are gated by `enable_v1_compat_writes` config (default true). CLI normalize is labeled as legacy compatibility. |
| G. Integrator UX improvements | **Done for MVP** | Per-tenant API keys and owner-scoped query checks landed. Quickstart, curl examples, HMAC verification example, and supported-path docs are in README. SDKs and dashboards are still pending. |
| H. Verification and operational hardening | **Done for MVP** | Smoke path proven (P0). Status/provenance usable (P1). Restart/reclaim, idempotency, tenant isolation, and export lifecycle tests added (P3). CLI/V1 compatibility story frozen with config gating (P2). |

## 3. Milestone Status

| Milestone | Status | Update |
|---|---|---|
| 1. Canonical naming and provider model | **Done** | No longer a planning blocker. |
| 2. Durable control plane runtime | **Done** | Durable ingestion/export/stream/materialization state is in Postgres. |
| 3. V2-authoritative backfill ingest | **Done for API** | Backfill worker writes V2 Bronze first, then compatibility projection. |
| 4. V2-authoritative streams | **Done for API** | Stream flushes create `ingestion_runs`, write V2 Bronze/checkpoints, and enqueue materialization. |
| 5. Bronze-driven materialization pipeline | **Done for planned coverage** | PR #241 completed all listed Gold datasets. Remaining work is correctness/provenance hardening, not missing coverage. |
| 6. V2-backed reads and compatibility cutover | **Partial** | V2-backed reads and no-run normalize removal are done. V1 compatibility writes and CLI V1 paths remain. |
| 7. Integrator polish and operational hardening | **MVP subset active** | Do quickstart/smoke/provenance first. SDKs and broader project UX can wait. |

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
- `/v1/datasets/:name/completeness` and `/v1/datasets/:name/status` are not yet
  tenant-scoped; tenant requests are rejected instead of returning global
  completeness.
- Some Gold tables do not carry `owner_id` directly. Isolation is enforced via
  target ownership checks and target-scoped queries, not database row-level
  ownership on every Gold row.

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

- API ingestion and stream workers still convert V1-shaped adapter output with
  `v1_tx_to_v2_raw()` before V2 persistence.
- API workers still perform best-effort V1 compatibility writes using
  `save_transactions()` and V1 checkpoints.
- `adapters/src/dual_write.rs::materialize_silver_datasets()` still exists for
  legacy/manual paths.
- CLI direct ingest still uses V1-first dual-write helpers in some paths.
- CLI `normalize` still parses V1 transactions, writes `ledger_entries`, and
  calls `materialize_silver_datasets()`.
- V1 tables and parser-version names remain for compatibility and tests.

MVP decision: do not block shipping on deleting all V1 code. For the first
usable release, make the API workflow the supported path and label CLI
normalize/legacy V1 projection as compatibility. Then remove or gate the
remaining compatibility writes once the API smoke path is proven.

## 7. Active MVP Plan

### MVP Target

A team member should be able to:

1. configure Postgres and providers,
2. create or use a tenant-scoped API key,
3. register a wallet target,
4. enqueue ingestion,
5. observe job/materialization status,
6. query `wallet_ledger`, `balance_history`, and at least one chain-specific
   Silver dataset,
7. export a dataset to CSV or JSONL,
8. repeat the flow after an API restart without losing jobs.

This is enough to be "actually usable" before SDKs, dashboards, or broader
project/org management.

### P0: Prove The API Happy Path

Deliver one checked-in or temporary smoke workflow:

- seed/create a tenant API key,
- register a wallet target,
- enqueue ingest for one supported network,
- wait for ingestion and auto-materialization,
- query `wallet_ledger` and `balance_history`,
- create and download a dataset export,
- show callback signing headers when `callback_hmac_secret` is configured.

Prefer a short shell script plus README quickstart over a large new framework.
The script can be local-only and can assume `docker-compose up -d`.

Acceptance criteria:

- a fresh local DB can run the flow,
- failures are visible and actionable,
- no one needs to read `api/src/main.rs` to operate the MVP.

### P1: Make Status And Provenance Usable

Fix the status surfaces that a user will naturally inspect:

- add tenant-scoped dataset status/completeness or document an explicit
  admin-only status endpoint for MVP,
- upsert Gold `dataset_completeness` when Gold records are written,
- make export job provenance useful for Gold datasets,
- ensure dataset version IDs are present for Gold rows written by the pipeline.

Acceptance criteria:

- dataset export status does not imply unknown/stale completeness for newly
  materialized Gold data,
- tenant users can inspect their own target's materialization state without
  seeing global state.

### P2: Freeze The Supported Compatibility Story

Make the supported path unambiguous:

- document API ingestion + auto-materialization as the supported MVP path,
- mark CLI `normalize` as legacy compatibility unless it is moved to the
  Bronze-native `ingestion_run_id` flow,
- add a config flag or documented operational switch for best-effort V1
  compatibility writes if operators need to disable them,
- keep V1 reads/writes only where they support rollback or compatibility.

Acceptance criteria:

- a user cannot accidentally choose the stale V1 normalize path thinking it is
  the recommended V2 pipeline,
- disabling optional V1 projection is either proven safe for the MVP path or
  clearly documented as not yet supported.

### P3: Add Minimal Operational Hardening

Focus on the exact MVP path:

- restart/reclaim test for ingestion -> materialization -> export,
- idempotency check for re-running materialization on the same ingestion run,
- one export test covering a Gold dataset,
- one tenant isolation test covering a forbidden target query/export.

Acceptance criteria:

- duplicate worker claims do not duplicate visible Gold/export side effects,
- tenant-scoped API keys cannot query or export another target.

### P4: Minimal Integrator UX

Do the smallest useful documentation work:

- quickstart for local Postgres + API,
- curl examples for target registration, ingest, job polling, dataset query,
  and export,
- callback HMAC verification example,
- a short "supported MVP path vs legacy compatibility path" note.

Do not prioritize SDK generation, dashboard work, enterprise project/org
management, or broad target presets before the MVP flow is proven.

## 8. Updated First PRs

The old suggested first PRs are no longer correct. The following are the best
next slices:

1. **MVP smoke workflow and docs**
   - Add a local quickstart and smoke script for the API path.
   - Include tenant API key setup, target registration, ingest, query, export,
     and optional callback signing.

2. **Gold status/provenance cleanup**
   - Upsert Gold completeness for all six Gold datasets.
   - Ensure export provenance is populated for Gold exports.
   - Make tenant-scoped dataset status/completeness available or explicitly
     admin-only with a documented replacement.

3. **CLI/V1 compatibility clarification**
   - Either route CLI normalize through Bronze-native `ingestion_run_id`
     materialization or label it as legacy.
   - Add a compatibility-write flag if operators need to run API ingestion
     without V1 projection.

4. **Focused hardening tests**
   - Add restart/reclaim and idempotency coverage for the MVP path.
   - Add one cross-tenant forbidden query/export test.

5. **Gold semantic validation**
   - Validate `balance_history` seeding, HL PnL/trade grouping, and
     `pool_snapshots` semantics against representative fixtures.
   - Treat protocol TVL as limited until pool token/reserve derivation is
     validated.

## 9. Definition Of Done For MVP

The MVP is ready when:

- the documented API path runs successfully on a fresh local environment,
- ingestion, materialization, query, and export survive an API restart,
- all six Gold datasets are materialized from Silver during the API path,
- wallet and dataset reads are tenant-scoped for DB-backed API keys,
- users can see enough status/provenance to trust a completed job/export,
- V1 compatibility behavior is documented and does not surprise operators.

Out of scope for MVP:

- deleting all V1 tables/code,
- full SDK generation,
- dashboard/frontend work,
- enterprise org/billing management,
- exhaustive reorg/finality modeling.
