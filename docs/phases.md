# Velocity — Phase-wise Implementation Plan (v2)

> Each phase produces a working, testable increment.
> v2 changes from v1: Phase 2 split into 2a/2b/2c. Phase 4 (Time Machine) moved before Phase 5 (Query). New Phase 4.5 (Operational tooling). Concrete acceptance criteria for Phase 10.

## Implementation status (as of 2026-05-20)

**Phases 0 through 10: shipped.** The codebase has full CRUD, all
auth strategies (JWT/OIDC/API key/composite), advanced access control
(7 layers including RLS + field masking + cross-schema RBAC), time
machine with warm-tier export and query, archive lifecycle with purge,
query DSL + 3-tier search with blue-green rebuild, audit chain with
verifier, central log pipeline with anomaly detection, observability
(SLO PrometheusRules + metrics), CLI with 18+ commands, and the React
admin portal. See per-phase `— **shipped**` markers and the
[Milestone Summary](#milestone-summary) table for evidence.

**Phase 11 (hardening): planned.** Production-readiness work — HA
failover drills, load testing, formal security audit, comprehensive
runbooks beyond the two skeletons in `runbooks/` — has not started.
Feature scope is complete; the gap is operational maturity, not
functionality.

---

## Pre-Phase — Architectural Decisions (1 week) — **shipped**

**Goal:** Resolve the foundational decisions before code starts. Decisions are recorded as ADRs in `decisions.md`.

### Deliverables

- [ ] ADR-001: SchemaRegistry consistency model (chosen: informer per replica)
- [ ] ADR-002: CDC mechanism (chosen: outbox pattern)
- [ ] ADR-003: Failure mode matrix (deny by default for auth/access)
- [ ] ADR-004: Time machine tiered storage (hot/warm/cold)
- [ ] ADR-005: Audit chain construction (DB stored procedure)
- [ ] ADR-006: Concurrency primitive (arc_swap)
- [ ] ADR-007: Database connection role (non-superuser)
- [ ] ADR-008: Kafka topic strategy (per-domain)
- [ ] ADR-009: Pagination (cursor + offset hybrid)
- [ ] ADR-010: Multi-tenancy model

These five decisions cost hours now and save months later. Do not start Phase 0 until they are settled and documented.

---

## Phase 0 — Foundation (2 weeks) — **shipped**

**Goal:** Repository skeleton, CRD types, Postgres provisioning wired end-to-end. Proves operator ↔ Postgres works with non-superuser role (ADR-007).

### Deliverables

**Repository structure:**

```
velocity/
├── crates/
│   ├── velocity-types/
│   ├── velocity-core/          # shared API library (auth, registry, config, …)
│   ├── velocity-operator/
│   ├── velocity-data-api/      # data plane (links velocity-core)
│   ├── velocity-platform-api/  # admin/UI + audit + SPA (links velocity-core)
│   ├── velocity-search/        # Tier-3 search + CDC (links velocity-core)
│   ├── velocity-warm-reader/
│   ├── velocity-typesense/
│   ├── velocity-log-processor/
│   ├── velocity-log-collector/
│   ├── velocity-cli/
│   ├── velocity-webhook/
│   └── velocity-archive-worker/
├── charts/
├── crds/
├── migrations/
├── portal/
├── tests/
├── runbooks/
└── docs/
```

**velocity-types** — all CRD struct definitions, schemars-derived JSON schemas, `cargo run --bin generate-crds` outputs to `crds/`.

**velocity-operator** (Phase 0 scope):
- HierarchyOperator skeleton
- Watches `Organisation`, `Application`, `Domain`
- Provisions Postgres schemas on Domain creation
- Creates non-superuser `velocity_api` role and per-domain roles (ADR-007)
- Verifies `BYPASSRLS=false` on `velocity_api` at startup (fail loudly if misconfigured)
- Placeholder `PolicyTree` (no merge logic yet)
- Leader election from day one
- `/healthz` and `/readyz` endpoints

**Platform schema migrations:**
- `platform.schema_definitions` (mirror, informational)
- `platform.field_definitions`
- `platform.event_log` (partitioned monthly)
- `platform.audit_log` + `platform.audit_chain_state`
- `platform.audit_insert()` stored procedure (ADR-005)
- `platform.api_keys`
- `platform.role_bindings`
- `platform.sessions`
- `platform.idempotency_keys`
- `platform.archive_runs`
- `platform.purge_requests`

**Infrastructure:**
- CloudNativePG operator + Postgres cluster (HA from day 1, even in dev)
- Redis (single in dev, Sentinel in staging+)
- Kafka KRaft (3 brokers in staging+)
- Helm chart skeleton

**Acceptance criteria:**
- Apply `Domain` CRD → Postgres schema exists with correct roles
- API server starts only when `velocity_api` role exists and has `NOBYPASSRLS=true`
- Apply same Domain twice → no error (idempotent)
- Operator restarts mid-reconcile → recovers without manual intervention

---

## Phase 1 — Core CRUD (3 weeks) — **shipped**

**Goal:** Apply a `SchemaDefinition` → working REST API with CRUD, validation, soft delete, optimistic locking.

### Deliverables

**SchemaOperator:**
- Watches `SchemaDefinition`
- `DdlBuilder`: generates DDL with auto-provisioned columns, partial unique indexes, mandatory triggers
- `MigrationDiffer`: safe ops applied; breaking ops blocked unless approved annotation
- Provisions main table, history table, outbox table (for Tier-3, even if not used yet)
- Status subresource updates: `Provisioning` → `Ready`

**velocity-api** (Phase 1 scope):
- Bootstrap: connect as `velocity_api`, verify `NOBYPASSRLS`
- Dynamic routing from informer (kubernetes informer fed registry per ADR-001)
- Use `arc_swap::ArcSwap` for registry (ADR-006)
- Readiness gate: traffic blocked until first full informer sync
- Generic handlers: list, create, get_one, update, delete
- `SET LOCAL ROLE` + `SET LOCAL app.current_user` on every transaction
- Validation: type, required, enum, range, pattern
- CEL with 10ms timeout (ADR / S2)
- Soft delete
- Optimistic locking with version column
- Idempotency-Key support (per ADR / U2)
- Cursor + offset pagination (per ADR-009)
- Consistent error response shape
- Structured JSON logging
- `X-Request-ID` propagation

**Validating webhook:**
- `velocity-webhook` binary
- HA: 3 replicas, anti-affinity
- Most CRDs: failurePolicy: Fail
- `Organisation`, `Application`, `Domain`: failurePolicy: Ignore (recovery escape)
- Validates: namespace match, quota, CEL syntax/size/depth, cross-domain refs (ADR-010 multi-tenancy)

**Acceptance criteria:**
- Apply SchemaDefinition → table provisioned → CRUD works
- Try to apply with bad CEL → webhook rejects with clear error
- Submit same Idempotency-Key twice → second returns cached response
- Soft-delete a record → re-create with same unique value → success
- Update with wrong version → 409 Conflict
- Connection role check fails → API server refuses to start (loud failure)

---

## Phase 2a — JWT + Basic Auth (2 weeks) — **shipped**

**Goal:** JWT authentication, route-level RBAC, RoleBinding (without scope filters). Unblocks Phase 3.

### Deliverables

**AuthStrategy CRD** (JWT-only in this phase):
- Operator resolves and caches in registry
- Validating webhook checks `strategyRef` existence

**JWKS cache:**
- Per-issuer cache
- Background refresh every 5 min
- Cache hit on JWKS endpoint unavailable (cryptographically safe per ADR-003)

**Auth middleware:**
- JWT extraction, verification (multi-issuer support)
- Claim mapping with transforms (JSONPath + prefix_strip, scope_to_roles, lookup, regex_extract, static_append)
- Build `Identity{actor_id, roles, attributes, strategy, issuer}`

**Redis revocation:**
- `revoked_actors` set
- Operator writes on RoleBinding deletion
- Fail mode per ADR-003 (deny by default, failOpen optional)
- Audit log records fail-mode applied to each request

**Route-level RBAC:**
- Layer 1 only — check schema.access.roles for operation
- No row filter, no field filter, no ABAC yet (Phase 2b)

**RoleBinding operator:**
- Syncs to `platform.role_bindings`
- Expiry enforcement
- Notifies Redis on revoke

**Acceptance criteria:**
- Valid JWT → request succeeds
- Expired JWT → 401
- Wrong issuer → 401
- Revoke actor → subsequent requests fail within seconds
- Kill Redis → requests denied (default) or allowed (if failOpen)
- Audit log records fail-mode used

---

## Phase 2b — Advanced Access Control (2 weeks) — **shipped**

**Goal:** Field filtering, row filtering, ABAC (CEL), Postgres RLS.

### Deliverables

**Layer 2: ABAC (CEL):**
- Compile CEL at schema load time
- Evaluate with 10ms timeout (ADR / S2)
- Deny on timeout or error

**Layer 3: Cross-schema RBAC (review fix):**
- Before adding joins/includes, verify identity has read on target schema
- Test case: actor with `procurement-reader` cannot include `supplier` data
- **Status: landed with Phase 5.** Cross-schema include semantics shipped together with the RBAC gate: a query that references a target schema the actor does not hold `read` on returns `403 CROSS_SCHEMA_ACCESS_DENIED` before SQL is built. Error code is defined in `crates/velocity-core/src/error.rs`; covered by integration tests in `crates/velocity-data-api/tests/` (the data-plane query path).

**Layer 4: Row filter:**
- Inject scope from RoleBinding into every query (cannot be removed)
- Test: actor in `region=west` cannot see records from `region=east`

**Layer 5: Field filter (read + write):**
- Strip fields per role on response
- Reject payload fields actor cannot write
- Test: write payload with forbidden field → 403

**Layer 6: Field masking:**
- Strategies: partial, hash, range, redact
- Per-field, per-role

**Layer 7: Postgres RLS:**
- Operator generates RLS policies from schema row filter declarations
- Verify `BYPASSRLS=false` on connection role (already done in Phase 0)
- Test: bypass app, connect to Postgres as `velocity_api` directly → RLS blocks

**Acceptance criteria:**
- All 7 layers verified independently (Layer 3 deferred to Phase 5 — see above)
- Role with read access on parent but not child schema → include rejected *(Phase 5 gate; not currently exercised because includes don't exist)*
- Postgres-direct query as `velocity_api` → RLS enforced
- ABAC CEL with deliberate infinite loop → terminated at 10ms

---

## Phase 2c — Additional Auth Strategies (1 week) — **shipped**

**Goal:** OIDC, API key, Composite strategies.

### Deliverables

- OIDC pipeline: session cookie, authorization code exchange, PKCE
- `/auth/callback` route, session store in `platform.sessions`
- OIDC userinfo claim resolution
- API key pipeline: SHA256 hash, IP allowlist, scope check
- API key format: `vel_{env}_{32 bytes base64}` (256 bits entropy per review fix S3)
- Composite strategy: try in order, first match wins

**Acceptance criteria:**
- OIDC redirect flow works browser end-to-end
- API key invalid → 401
- API key from disallowed IP → 401
- Composite: JWT fails, falls through to API key, succeeds

---

## Phase 3 — Time Machine (2 weeks) — **shipped**

**Goal:** History tables, event log, point-in-time query, restore. (Moved before Query — review fix Ph2.)

### Deliverables

**History table provisioning:**
- DdlBuilder generates `{table}_history` partitioned by `occurred_at`
- Monthly partitions, 90-day hot retention
- Trigger function generated: writes history + outbox in one transaction

**Event log writes:**
- Generic handlers write to `platform.event_log` on every mutation
- JSON Patch computed on UPDATE
- Async writes (don't block response)
- Source tagging: api / operator-sync / import / migration

**Time Machine API:**
- `GET /{id}/history` — paginated
- `GET /{id}/history?at=T` — state at point
- `GET /{id}/diff?from=T1&to=T2` — field-level diff
- `POST /{id}/restore` — new event applying old state
- `GET /{id}/replay` — SSE stream
- `POST /{org}/{app}/{domain}/history/snapshot?at=T` — cross-entity snapshot

**Restore semantics:**
- Restore is a new event, not a rollback
- X-Reason required if configured
- Restore-of-restore allowed
- **No-op detection:** if target state == current state → 409 (review fix P6)

**Acceptance criteria:**
- Create → update × 3 → 4 history events
- `?at=T` returns correct state at each point
- Restore creates new event with marker `restored_from_version`
- Restore to current state → 409 RESTORE_NO_OP
- Large table: history queries do not slow main table queries

---

## Phase 4 — Time Machine Tiering (1 week) — **shipped**

**Goal:** Warm (S3 Parquet) tier with DuckDB queries. Cold tier interface (Glacier integration optional in this phase).

### Deliverables

**Operator nightly export job:**
- Identify oldest hot partition no longer in retention
- Export to S3 Parquet (`{schema}/{year}/{month}/`)
- Verify Parquet readable via DuckDB
- Detach + drop hot partition

**Warm tier query routing:**
- API server detects age of requested point
- 90-day hot → Postgres
- 5-year warm → DuckDB on S3 Parquet (2-10s latency)
- Cold → 202 + job_id (deferred fulfillment)

**Cold tier:** Just the API — `202 Accepted` with job_id. Glacier integration can wait until production need arises.

**Acceptance criteria:**
- Export job runs nightly, partition removed from hot
- Query 6-month-old state → DuckDB returns correct data
- Query 7-year-old state → 202 with job_id

---

## Phase 4.5 — Operational Tooling (1 week) — **shipped**

**Goal:** Operations team can manage Velocity in production. Drift detection, manual reconcile, status queries.

### Deliverables

**velocity-cli operational commands:**
- `velocity drift check` — detect orphan tables, schema mismatches, missing indexes
- `velocity drift quarantine <table>` — move orphan to quarantine schema
- `velocity reconcile <kind> <name>` — force operator reconcile
- `velocity status` — full hierarchy health
- `velocity audit verify --window <duration>` — chain verification

**Operator drift reconciliation:**
- Periodic full sweep (hourly)
- Compares state in Postgres to declared SchemaDefinitions
- Emits metric: `velocity_drift_detected_total`
- Logs WARN per drift

**Acceptance criteria:**
- Delete a SchemaDefinition but leave table → `velocity drift check` flags it
- Manually break audit chain → `velocity audit verify` detects it
- Force-reconcile a stuck schema → resolves

---

## Phase 5 — Query Engine & Search (2 weeks) — **shipped**

**Goal:** POST /query DSL, all three search tiers, cross-schema search.

### Deliverables

**Query DSL engine:**
- QueryBuilder: WHERE (nested AND/OR), ORDER BY, SELECT, JOIN, AGGREGATE
- Cursor pagination (ADR-009)
- Cross-schema RBAC check on includes (review fix S6)
- Whitelist validation: unknown fields, non-filterable, non-sortable all rejected
- All invariants enforced at build time

**Tier 1 — Postgres filters:**
- URL param filters auto-generated from field declarations

**Tier 2 — Postgres FTS:**
- `tsvector` generated column, GIN index
- Weighted by field weight: A/B/C/D
- `?q=` activates FTS, combined with filters

**Tier 3 — Typesense via outbox (ADR-002):**
- Operator provisions Typesense collection on schema apply
- CDC worker per Tier-3 schema reads outbox with `FOR UPDATE SKIP LOCKED`
- Periodic outbox cleanup
- Search index evolution: blue-green when searchable fields change

**Cross-schema search:**
- Unified Typesense collection
- Per-org partition keys
- Result diversity (`maxPerSchema`, `minPerSchema`)
- RBAC: only search schemas actor can read

**Acceptance criteria:**
- DSL: every operator type produces correct SQL
- DSL: unknown field → 400 with clear error
- DSL: SQL injection attempts fail (parameterized only)
- Outbox: kill CDC worker, write 100 records, restart worker → all 100 in Typesense
- Cross-schema search: actor without supplier access → no supplier results

**Phase 5 — shipped vs deferred:**

Shipped in 5a/5b/5c: DSL engine, Tier-1 filters, Tier-2 FTS with uniform
weighting, Tier-3 CDC with lazy collection provisioning, per-org
cross-schema collection, cross-schema RBAC. Three items from the original
deliverables list above were deferred to Phase 5d so the happy path could
land first: per-field FTS weights (A/B/C/D), operator-side eager Typesense
provisioning, and blue-green collection swap on searchable-field change.

---

## Phase 5d — Search Close-out (1 week) — **shipped**

**Goal:** Land the three Phase 5 deliverables that were intentionally
deferred from 5a/5b/5c. After this phase, Phase 5's deliverables list
matches what's actually in the binary — no asterisks.

### Deliverables

**Per-field FTS weights (A/B/C/D):**
- New CRD knob on `FieldSpec`: `ftsWeight: A | B | C | D` (defaults to
  `D` — keeps existing collections' ranking semantically unchanged)
- Webhook validation: weight only meaningful when `searchable: true` and
  field is a string/enum; reject otherwise
- `DdlBuilder` emits `setweight(to_tsvector('english', coalesce(<f>,'')), '<W>')`
  per field, concatenated with `||`, replacing today's flat
  `to_tsvector(... || ' ' || ...)`
- Migration diff: a weight change is a `__fts` definition change — same
  blast radius as adding a searchable field (rebuild the generated column),
  so it must go through the existing breaking-change annotation gate or
  be applied via `DROP COLUMN __fts; ADD COLUMN __fts ...` only when the
  table is empty

**Operator-side eager Typesense provisioning:**
- SchemaDefinition reconciler calls `TypesenseClient::ensure_collection`
  for any Tier-3 schema, same step that provisions the outbox table
- Per-org cross-schema collection (`<org>__cross`) ensured on first Tier-3
  schema in the org
- Failure mode (ADR-003 aligned): if Typesense is unreachable, reconcile
  fails loud — schema status reports `TypesenseUnavailable`, reconcile
  requeues with backoff. No silent fallback to "lazy create later"; the
  whole point is removing first-write latency
- CDC worker keeps its `ensure_collection` call as a defensive idempotent
  no-op so a manually-deleted collection self-heals on next write
- Operator gets new env vars (`VELOCITY_OPERATOR_TYPESENSE_URL`,
  `VELOCITY_OPERATOR_TYPESENSE_API_KEY`) — paired-or-neither, same shape
  as the API's pairing

**Blue-green collection swap on searchable-field change:**
- Introduce a stable Typesense alias per collection
  (`<org>__<app>__<domain>__<object>__<version>`) pointing at a versioned
  underlying collection (`…__r1`, `…__r2`, …)
- All reads (search handler, cross-search handler, CDC upserts) go
  through the alias name, not the underlying name
- Reconciler detects searchable-field-set or weight change by hashing the
  effective FTS spec; on mismatch:
  1. Create `…__r{n+1}` with the new schema
  2. Spawn (or enqueue) a backfill: stream every non-deleted row from
     Postgres into `…__r{n+1}` via Typesense bulk import
  3. Tail outbox writes are dual-written (`…__r{n}` AND `…__r{n+1}`) for
     the duration of the backfill so the new collection stays caught up
  4. Once backfill is done and lag is zero, atomic alias flip to
     `…__r{n+1}`
  5. After grace period (operator-tunable, default 24h), drop `…__r{n}`
- New CRD status field `searchIndex.activeRevision` reports the current
  underlying collection name — debuggable from `kubectl describe`
- Cross-schema collection follows the same pattern keyed on per-org
  searchable-spec hash

### Acceptance criteria

- Per-field weights: schema with `title: ftsWeight A` and `body: ftsWeight D`
  → query matching both ranks title-only document above body-only document
- Eager provisioning: apply a Tier-3 SchemaDefinition with Typesense up
  → collection exists before the first row is written; first /search hits
  return 200 (not "collection not found")
- Eager provisioning failure: apply with Typesense down → reconcile fails,
  schema status shows `TypesenseUnavailable`, no silent partial state
- Blue-green: write 10k rows, change a searchable field, observe alias
  flip → /search continues serving throughout (zero-downtime), final
  result set matches the new field schema
- Blue-green concurrency: writes during backfill land in BOTH collections
  → post-flip count matches pre-flip count + writes-during-backfill

### Non-goals (still deferred, callout for future work)

- Multi-language FTS (today's `__fts` is hard-coded `english`)
- Field-level facets / typo tolerance tuning per field
- Cross-collection joins inside Typesense

---

## Phase 6 — Audit & Central Logging (2 weeks) — **shipped**

**Goal:** Audit log chain working. LogFilterPolicy + LogRoutingPolicy operational. LogCollector shipping to Loki + S3.

### Deliverables

**Audit log writer:**
- Always use `platform.audit_insert()` stored procedure (ADR-005)
- Direct INSERTs blocked at PostgresGRANT level
- Records: actor, action, outcome, entity, fields_accessed, fields_changed, fail_mode (per ADR-003)
- Sensitive field detection from schema
- Denial auditing (every 403)

**Audit API:**
- Filterable by actor, schema, action, outcome, time, ticket_ref
- `GET /audit/verify?from=T1&to=T2` — chain integrity verification
- Audited itself

**Anomaly detection:**
- Operator scheduled checks: bulk readers, after-hours access, repeated denials
- Publishes to `velocity.alerts` topic

**LogFilterPolicy operator:**
- Watches LogFilterPolicy
- Distributes rules to LogProcessor via config map

**LogRoutingPolicy operator:**
- Watches LogRoutingPolicy
- Configures LogRouter destinations

**LogCollector DaemonSet:**
- Reads pod stdout from host paths
- Parses structured JSON (no regex)
- Ships to LogProcessor

**LogProcessor:**
- Enrichment: add velocity.{org,app,domain,schema} labels
- Rule evaluation: priority-ordered, keep-overrides-drop, sample/redact
- Routes to Loki, S3, Kafka

**Acceptance criteria:**
- Every request → audit entry
- Tamper one row in audit_log → `velocity audit verify` detects within 10s
- LogFilterPolicy drop rule → logs not in Loki
- Sensitive field redacted in log → value replaced with `***`

---

## Phase 7 — Observability (2 weeks) — **shipped**

**Goal:** Prometheus metrics, OpenTelemetry traces, SLOs, auto-generated Grafana dashboards.

### Deliverables

**Prometheus metrics (Axum middleware):**
- Schema-aware metrics with **bounded label cardinality** (review fix P7)
- `velocity_operations_total{schema, operation, outcome, actor_type, strategy}`
- `velocity_operation_duration_seconds{schema, operation}`
- `velocity_validation_failures_total{schema, field, rule}`
- `velocity_auth_attempts_total{schema, strategy, outcome}`
- High-cardinality fields → traces and logs, not metrics

**OpenTelemetry tracing:**
- Root span per request
- Auth, schema_resolve, rbac_check, validation, postgres, hooks spans
- Trace propagation to Kafka headers and HTTP hooks (W3C traceparent)
- Sampling per operation type (always trace errors and slow requests)

**SLO alerting:**
- Operator generates Prometheus recording rules from `schema.observability.slos`
- `PrometheusRule` CRDs auto-created
- P99 latency, error rate, availability

**Grafana dashboards:**
- Per-schema dashboard auto-created
- Panels: request rate, error rate, P99, validation failures, auth outcomes
- Business metrics from schema declarations

**Health endpoints:**
- `/health` — platform health
- `/health/schemas/{kind}` — per-schema health

**Acceptance criteria:**
- `/metrics` returns Prometheus exposition format
- Trace visible in Tempo with full schema/operation context
- SLO alert fires when P99 exceeds target for 5 minutes
- Dashboard auto-created on schema apply

---

## Phase 8 — Archive & Lifecycle (2 weeks) — **shipped**

**Goal:** ArchivePolicy working. Archive operator running on schedule. Purge lifecycle with approval.

### Deliverables

**ArchiveOperator:**
- Watches ArchivePolicy
- Trigger evaluation: age, field value, table size, CEL (with 10ms timeout)
- Cron schedule
- Batched, transactional archival
- Postgres cold schema provisioning
- S3 archival (Parquet + gzip)
- Max duration enforcement
- Metrics: `velocity_archive_records_total`
- Kafka event: `velocity.archive.completed`

**Purge lifecycle:**
- PurgeRequest CRD raised when records reach purgeAfter
- Kafka event: `velocity.purge.pending`
- 30-day notification
- Operator checks for approval annotation before purge
- Hard DELETE from archive store on approval

**Archive API:**
- `GET /{id}/archive` — full record from cold store
- `POST /archive/query` — DSL on archive
- `POST /{id}/unarchive` — restore to main table

**Version lifecycle:**
- VersionOperator manages status transitions
- Deprecated schemas: writes return 410 Gone with migration URL
- Sunset schemas: all requests 410 Gone

**Acceptance criteria:**
- Records older than threshold → archived
- Main table has stub row with archive_ref
- Archive query returns full record
- Unarchive removes stub, restores main row
- PurgeRequest without approval annotation → no purge action

**Phase 8 — shipped vs deferred:**

Shipped across slices 1–10:

- Slice 1: ArchivePolicy validation controller — schedule (5/6-field
  cron shape), trigger kind (age/field/tableSize/cel), destination
  (postgres-cold | s3), purgeAfter / maxDuration durations. Failed
  validation surfaces as `phase=Failed` + `False` conditions; no
  worker activity until valid.
- Slice 2: Operator provisions per-domain `<org>_<app>_<dom>_archive`
  Postgres schema and reader/writer/admin roles when an ArchivePolicy
  for that domain validates and `destination.backend == postgres-cold`.
  Shares the `sync_domain` advisory lock; `velocity_api` is granted
  membership in each role so `SET LOCAL ROLE` still works for archive
  reads (ADR-007).
- Slice 3: Archive mirror table provisioning — column-for-column
  `CREATE TABLE IF NOT EXISTS` per SchemaDefinition in the namespace.
  No FKs, no indexes, no `__fts` generated column. Surfaced in
  `status.mirroredTables` (sorted) and an
  `ArchiveMirrorsProvisioned` condition. This slice also renamed
  slice 2's `_cold` operator status fields to `_archive` to match the
  pre-existing `archived_at` / `archive_ref` system columns.
- Slice 4: `archive_batch` primitive — single-transaction CTE moves
  bounded rows from hot to archive mirror
  (`WITH picked → INSERT … ON CONFLICT (id) DO NOTHING → UPDATE hot
  SET archived_at = now() RETURNING id`). Hot row's `archived_at` is
  the source of truth for "moved"; archive copy is the durable home.
  Soft-delete filter (`deleted_at IS NULL`) chained so already-deleted
  rows don't migrate. Pure `build_archive_batch_sql` helper for
  identifier/bound validation without a DB.
- Slice 5: `velocity-archive-worker` driver loop — wakes every
  `tick_interval`, lists ArchivePolicies, runs `archive_batch` until
  caught up or `maxDuration` expires per Ready postgres-cold
  age-trigger policy. Patches `status.lastRunAt` (RFC 3339) and
  cumulative `status.recordsArchived`. Per-policy / per-table errors
  logged but don't abort the tick.
- Slice 6: `purge_batch` primitive + worker integration — hard
  `DELETE` from the hot table once `archived_at + purgeAfter` has
  elapsed. Same single-transaction CTE shape as slice 4. Adds
  `status.recordsPurged` counter.
- Slice 7: PurgeRequest controller — manual hard-delete of archived
  rows from `<…>_archive.<table>` gated by a human-applied
  `velocity.sh/approved-by` annotation. Lifecycle: spec validation
  → Approval gate (idles in `Pending` until annotated) → single-
  statement `DELETE FROM … WHERE archived_at < $olderThan` → status
  writes `phase=Ready`, `approved=true`, `approvedBy`, `purgedAt`,
  `purgedRecords`. Idempotent — re-applied request re-runs DELETE
  (no-op once drained).
- Slice 8: field / tableSize triggers — `ArchivePredicate` enum
  covering `Age { min_age }`, `Field { field, op, value }`
  (lt/le/gt/ge/eq/ne), and `Oldest` (no predicate; drain oldest by
  `created_at`). For tableSize triggers the worker pre-checks
  `pg_total_relation_size` and dispatches `Oldest` when over.
  `parse_byte_size_value` accepts bytes integer or
  N{B|KiB|MiB|GiB|TiB}.
- Slice 9: Archive API — `GET /{id}/archive` (single row),
  `POST /archive/query` (paginated, optional `archivedAfter` floor,
  default 100 / max 1000, `ORDER BY archived_at DESC, id`),
  `POST /{id}/unarchive` (clears `archived_at` on hot row, drops
  archive copy; 410 `ARCHIVE_HOT_ROW_PURGED` when the hot row has
  already been purged via `purgeAfter`). All three honour the same
  RBAC + field filter + masking as the hot endpoints.
- Slice 10: S3 Parquet destination —
  `s3_destination::archive_batch_to_s3` is the S3 sibling of slice 4.
  Three-phase: pick → upload to `object_store` under
  `<prefix>/<schema>/<table>/dt=YYYY-MM-DD/<uuid>.parquet` (Snappy) →
  mark source rows with `archived_at = now()` + `archive_ref = <key>`.
  MVP encodes every value as JSON text inside nullable Utf8 columns
  — queryable via DuckDB/Athena `CAST`, less efficient than typed
  Arrow columns (future polish). Worker reads `ARCHIVE_S3_BUCKET` +
  `AWS_REGION`; without them, s3-destined policies skip with a
  warning.

Deferred (explicit, not silent):

- **CEL trigger** — `cel-interpreter` plumbing + per-row evaluation
  not wired. Slice 1 still validates a CEL trigger spec, but slice
  8's predicate enum doesn't carry a `Cel` variant yet, so the
  worker silently skips policies whose `trigger.type == cel`. Picked
  up when CEL hot-path use-cases land.
- **Sharded archive workers with `FOR UPDATE SKIP LOCKED`** — slice 4
  documented single-writer assumption; horizontal scale + lock
  contention is a slice 11+ concern.
- **S3 orphan recovery sweep** — a crash between Parquet upload and
  hot-row marking in slice 10 leaves an orphan object that the next
  tick re-picks (different uuid). Reaping orphan parquets is "slice
  12" per the slice 10 commit.
- **Cron-precise scheduling** — slice 5 ticks on a fixed interval
  with `min_run_interval` debounce, not on actual cron firing
  instants. Calendrical scheduling lands when load demands it.
- **Typed Arrow column schemas keyed off `FieldKind`** — slice 10
  uses all-Utf8 columns as a deliberate MVP shortcut.
- **Version lifecycle (VersionOperator, `410 Gone` for deprecated /
  sunset schemas)** — listed in the Phase 8 plan above but not yet
  in the binary. Schema deprecation today is informational only; the
  status transitions and the 410 response path are a future slice
  alongside the multi-version migration tooling.

---

## Phase 9 — CLI (1 week) — **shipped**

**Goal:** `velocity` binary distributed as single static binary.

### Deliverables

**velocity-cli crate** with full command set:
- apply, get, describe, delete, diff
- logs (kube-rs pod log streamer; see crates/velocity-cli/src/logs_cmd.rs)
- record query, record export (nested under `record` since records are
  the queried/exported entity; functionally equivalent to top-level
  `velocity query` / `velocity export` from the original spec)
- history
- restore, grant, revoke
- api-key (create, revoke, rotate)
- archive (request, approve, status, list)
- drift, reconcile, status
- audit verify
- health, metrics, slo
- context (list/use/add)
- version

**Build pipeline:**
- musl static link, x86_64 + aarch64 (Linux + macOS)
- GitHub Actions release on tag
- Install script

**Authentication:**
- OIDC device flow OR token in `~/.velocity/config`
- Token rotation supported

**Acceptance criteria:**
- All commands work end-to-end against a test deployment
- `velocity apply` with stdin works
- Output formats: table, json, yaml
- Single binary < 20MB

---

## Phase 10 — Admin Portal (2 weeks) — **shipped**

**Goal:** React SPA for managing Velocity objects. Visual editor with live YAML preview.

### Shipped

`portal/` — React 19 + Vite 6 + TypeScript + Tailwind 3 SPA, served by nginx
with same-origin proxy to `velocity-api`.

**Layout:** dark theme, amber accent, monospace, three-panel shell (sidebar /
main / context).

**Views backed by the API:**
- Overview — schema count, registry ready badge, recent audit, schema list
- Hierarchy browser — org → app → domain → object/version tree
- Schema list + detail (detail links out to records / audit / `velocity get`)
- Records list, JSON-body create / edit / soft-delete, query / search
- Time machine — version timeline, diff viewer, restore-to-version
- Audit log — filters (actor / schema / entity / op / outcome) + chain verify
- Health — readyz probe + registered-schema table
- Metrics — link-out to per-schema Grafana dashboards via portal config

**CRD editors (form + live YAML preview + Copy YAML / Copy command):**
- `SchemaDefinition` — full visual editor: identity, policy refs, fields with
  per-field flags + sensitivity, CEL validation rule
- `AuthStrategy` — kind picker (oidc / jwt / api_key / composite), revocation
  fail-open toggle per ADR-003
- `RoleBinding` — subjects + role + optional ABAC scope
- `ApiKey` — namespaced scopes, expires-at; documents the one-shot reveal
- `LogFilterPolicy`, `LogRoutingPolicy` — YAML rule blocks
- Logging — pipeline diagram + links to the policy editors

The portal does NOT proxy CRD writes; it generates a manifest you apply with
`velocity apply -f -`. Listing existing CRDs requires `velocity get` /
`kubectl get` because the API server has no admin CRD read endpoints today.

**Deployment:**
- `portal/Dockerfile` — node 22 build → nginx 1.27-alpine serve
- Same-origin proxy of `/api`, `/auth`, `/version`, `/healthz`, `/readyz` to
  `velocity-api`; nginx resolver reads `/etc/resolv.conf` so the image works
  in Docker (127.0.0.11) and Kubernetes (cluster DNS) without recompilation
- Runtime config served at `/config.json` from a ConfigMap (default OIDC
  AuthStrategy, Grafana URL, environment banner)
- Helm: added to `charts/velocity/` under `portal.enabled` with Deployment,
  Service, ConfigMap, optional Ingress
- CI: `build_portal` + `merge_portal` jobs in `.github/workflows/docker.yml`
  publish `ghcr.io/<owner>/velocity-portal` as a multi-arch manifest on tag
  pushes, mirroring the Rust binary release flow

**Tests:** Vitest + jsdom — API client (fetch wrapper, error mapping, 401
event dispatch, 204 handling, path round-trip) + YAML manifest-shape pins.

### Deferred (out of scope for v1 portal)

- AuthStrategy / RoleBinding / ApiKey / LogPolicy **list** views (no admin
  read API exists; would either need new endpoints or a browser-side k8s API
  client)
- Live central log stream (no websocket endpoint on `velocity-api`)
- Pre-populating editors from existing CRDs (depends on list endpoints above)

---

## Phase 11 — Hardening & Production Readiness (3 weeks) — **planned**

**Goal:** Production-ready. Concrete acceptance criteria.

### Deliverables

**Security audit:**
- `cargo audit` clean
- Dependency scan
- Postgres connection role verified non-superuser
- API key entropy verified
- Audit chain integrity verified end-to-end
- All sensitive fields confirmed redacted in logs

**Resilience testing:**
- Kill API server mid-request → in-flight completes or 503 retry
- Operator reconcile idempotent: apply same CRD twice → no-op
- DB pool exhaustion → requests queued not dropped
- Kafka unavailable → hooks queued in Redis, retried on recovery
- Redis unavailable → revocation fails closed (default)
- Typesense unavailable → search falls back to Tier-2

**Load test scenarios (concrete acceptance):**

```
Steady state:
  5000 RPS, 1 hour, 80% read / 20% write
  P99 create < 200ms
  P99 read < 100ms
  Zero data loss
  Zero auth bypass
  Audit chain valid end-to-end

Burst:
  0 → 10000 RPS over 30s, sustained 5 min
  HPA + KEDA scales up within 60s
  No request loss
  P99 < 300ms during burst

Schema evolution under load:
  Apply v1 → v2 schema while running 3000 RPS
  Zero downtime
  No request fails
  CDC worker re-syncs to new collection within 60s

Failover under load:
  Kill Postgres primary at 5000 RPS
  Recovery within 30s
  Failed requests during failover < 0.1%
  No data loss
```

**Documentation:**
- README quickstart (apply a schema in < 5 minutes)
- Architecture doc
- API reference (auto-generated from OpenAPI)
- CRD reference (auto-generated)
- Runbooks per `operations.md`

**Observability validation:**
- Every component emits metrics, traces, logs
- SLO alerts tested (exceed threshold → alert fires)
- Audit chain verified nightly (cron job)

---

## Phase 12 — Per-Object Deployments, Anonymous Mode, UI Overhaul — **planned**

> **Gated on [ADR-011](decisions.md#adr-011-dedicated-api-deployment-per-velocity-object)
> being Accepted.** ADR-011 is currently *Proposed*; it carries five open
> decisions (granularity, the platform/data API split, idle-pod cost,
> connection pooling, routing). 12a does not start until those are
> settled. 12b and 12c are independent of 12a and can proceed in
> parallel once their own prerequisites are met.

This phase is **three independent work-streams**, each with its own
acceptance criteria so any one can be re-ordered or vetoed without
re-cutting the others.

### Phase 12a — Per-Velocity-Object API Deployments (control-plane re-architecture) — **in progress**

> **Topology decision (2026-05-24, see [ADR-011](decisions.md) "Final service
> topology"):** the data plane splits into separate binary crates over a
> shared `velocity-api` library — `velocity-platform-api` (admin/UI/CRD-write),
> `velocity-data-api` (per-domain CRUD/query/time-machine/archive, Postgres
> only), and `velocity-search` (all search — per-schema/domain/cross-domain/
> cross-org — plus the CDC outbox→Typesense workers and collection mgmt).
> `velocity-warm-reader` already exists. `VELOCITY_API_MODE` is retired.
> **Refactor sequence:** (1) ✅ shared server bootstrap extracted to
> `velocity_api::server::bootstrap_common`, `main.rs` a thin consumer. (2) ✅
> `velocity-platform-api` binary — admin/UI: `router::build_platform_api`
> (index/version/platform-audit) + `platform_objects` admin CRD read/write/delete
> (kube `DynamicObject` SSA, webhook in path, token-gated) + embedded UI. No
> data CRUD. (3) ✅ `velocity-data-api` binary — `router::build_data_api`,
> Postgres-only; shared-default (all-ns) or dedicated (scoped) by
> `VELOCITY_API_NAMESPACE`. (4) ✅ `velocity-search` binary —
> `router::build_search_api` (per-schema + cross-org) + CDC outbox→Typesense.
> All four binaries compile clippy-clean; workspace green.
>
> **Step 5 — done.** Dockerfile builds all binaries via `BIN` (documented);
> `velocity-api` is now **lib-only** (`[[bin]]` + `src/main.rs` removed); chart
> repurposes the `api` Deployment to run `velocity-platform-api` and adds
> `data-api-deployment.yaml` (shared-default, all-ns) + `search-deployment.yaml`
> (+ services); `api-ingress.yaml` routes one host by longest-prefix
> (`/search`→search, `/api/platform`→platform, `/api`→data, `/`→platform UI);
> `velocity-search` mounts under `/search` natively (auth `schema_path_from_uri`
> strips the prefix); operator dedicated orchestration (`operator.dataApi.enabled`)
> uses the computed `velocity-data-api` image; `docker.yml` matrix builds the
> three binaries (portal jobs gated off — SPA embedded in platform-api). Helm
> renders clean (default + orchestration-enabled); workspace clippy clean; 358
> lib tests pass.
>
> **Step 4b — done (link-level isolation via Cargo feature, not relocation).**
> Rather than physically move 50 KB of hot-path code across crates (high
> breakage risk), search is gated behind a `search` cargo feature on the
> `velocity-api` lib (`default = ["search"]`, `velocity-typesense` optional).
> Gated: `handlers::search`/`cross_search` + `SearchRequest`, `cdc`,
> `typesense`, `AppState.typesense` + `with_typesense`, `router::build_search_api`,
> and the search-route handlers in `build`/`build_data_api` (which resolve to
> `platform_only` when off). `velocity-data-api` and `velocity-platform-api`
> depend with `default-features = false`; `velocity-search` keeps it on. Since
> the Dockerfile builds per-binary (`cargo build --bin ${BIN}`), the
> data-API/platform-API images never compile search or the Typesense client in
> — link-level isolation in the deployed artifact. Verified: lib compiles +
> clippy clean both feature sets; 358 tests (search on) / 356 (off);
> `velocity-data-api` builds with no Typesense in its graph. (`platform_objects`
> admin code already lives in the platform-api crate.) Shared-mode domains →
> shared default `velocity-data-api`; platform-API does no data CRUD
> (ADR-011 "Final service topology").

> **Step 6 — done (2026-05-25): physical relocation + crate rename, superseding
> the Step-4b feature flag.** The shared library was renamed `velocity-api` →
> **`velocity-core`**, and the tier-specific code was physically moved out of it
> into the binary crates (each now lib+bin):
> - `velocity-search` owns `cdc` + `typesense` + the search handlers +
>   `build_search_api` (+ `SearchState`). The `search` Cargo feature is
>   **deleted** — isolation is now structural (a crate boundary), not a flag.
> - `velocity-data-api` owns the data plane: `handlers` (CRUD), `dsl`, `tiering`,
>   `time_machine`, `archive_handlers`, `event_log`, `idempotency`, `session`,
>   `build_tiered_reader`, the data router (+ `DataState`).
> - `velocity-platform-api` owns `platform_handlers` + `audit_query` +
>   `static_files` (the SPA, now embedded from `crates/velocity-platform-api/static/`)
>   + `build_platform_api` (+ `PlatformState`).
> - `velocity-core` is the pure shared foundation: auth (+handlers/informer),
>   `SchemaRegistry`, config, `cursor`, audit-write, the schema/access model
>   (validate/field_filter/masking/policy/row_filter/rbac/query), `handler_util`,
>   health, metrics, `server` bootstrap, and `build_auth`. `CursorSigner` split
>   into its own `cursor` module; shared handler helpers (`resolve_schema`,
>   `audit_if_denied`, …) lifted into `handler_util` with state-free signatures.
> `cargo tree -e normal` confirms no tier links another tier; `velocity-typesense`
> is linked only by `velocity-search`. Integration tests moved with their code.
> Clippy `-D warnings` clean; lib tests core 274 / data-api 65 / search 1 /
> platform-api 22. The Postgres role `velocity_api` and `VELOCITY_API_*` env
> prefix are unchanged (only the crate path `velocity_api::` → `velocity_core::`).

> Landed (compiling, clippy-clean, unit-tested): `DomainSpec.deployment`
> block + `DomainStatus.dataApiDeployment` (`velocity-types`, CRDs
> regenerated); the `VELOCITY_API_MODE=platform|data` split
> (`config.rs`, `router::build_data_api`, `main.rs`) where data mode scopes
> the informer to one namespace and answers `PLATFORM_ONLY` on
> cross-schema/platform routes; the operator **workload orchestrator**
> (`workload.rs`) that server-side-applies a Deployment + Service + HPA
> (`minReplicas≥1`) + per-domain Ingress path, owner-ref'd to the Domain for
> GC, wired into the `Domain` reconciler; operator config
> (`VELOCITY_OPERATOR_DATA_API_*`) + RBAC (deployments/services/HPA/ingresses)
> + chart wiring (`operator.dataApi.*`).
>
> **Env-secret projection + per-domain DB credentials — landed.** The
> operator mints a per-domain LOGIN role `{schema}_api` (NOSUPERUSER,
> NOBYPASSRLS, member of reader/writer/admin) via
> `provisioner::ensure_data_api_login_role`, and projects the data-API env
> Secret into each domain namespace (`workload::project_env_secret`): it reads
> the chart's source Secret (`data-api-env-secret.yaml`), reuses-or-mints the
> password (never rotates under a running pod), and writes an owner-ref'd
> Secret the Deployment consumes via `envFrom`. Unit-tested (SQL builder +
> hex-password validator); chart renders verified.
>
> **Deferred within 12a (not yet built):**
>
> **(1) PgBouncer — PARKED (2026-05-24).** Connection pooling in front of CNPG
> is deliberately deferred, not in progress. Rationale and the design to pick
> up later: because per-domain roles (`{schema}_api`) are **minted at runtime**
> by the operator, a static `userlist.txt` won't work — PgBouncer must use
> `auth_type = scram-sha-256` + `auth_query` against a `SECURITY DEFINER`
> lookup function over `pg_shadow`. That function (and the `pgbouncer_auth`
> role) must be created by a **superuser at CNPG bootstrap**
> (`initdb.postInitApplicationSQL`), since the operator connects as a
> non-superuser (ADR-007) and cannot grant itself `pg_shadow` access. Plan
> when resumed: chart pieces (Deployment + Service + ConfigMap, disabled by
> default) + a bootstrap-SQL artifact for ops + flip the data-API source
> secret's `VELOCITY_API_PG_HOST`/`PORT` to the PgBouncer service (`:6432`).
> Until then, data-API pods connect directly to Postgres; this only matters
> under real connection-fan-out load.
>
> **(2) KEDA traffic-scaling** — a CPU-target HPA is the always-available
> baseline today; the KEDA `ScaledObject` (Prometheus RPS trigger) is a
> follow-up.
>
> **(3)** the cluster integration test (apply dedicated Domain → Deployment
> created → CRUD via domain path → cross-search 404 → delete Domain → GC).


**Goal:** The operator materialises a dedicated **data-API** Deployment
per Velocity object (a `SchemaDefinition` + its related CRDs). A shared
**platform-API** retains the ADR-001/006 registry for cross-schema and
platform concerns. See ADR-011 for the full analysis.

**Decisions to lock first (ADR-011):** granularity (per-Domain
recommended vs per-SchemaDefinition vs opt-in hybrid); platform/data
split; min-replicas vs scale-to-zero; PgBouncer; routing strategy.

**Deliverables:**

- **`SchemaDefinition` (and/or `Domain`) gains a `deployment` block** in
  `velocity-types`: `mode` (`shared` | `dedicated`), replica bounds,
  resource requests/limits, scale-to-zero toggle. Regenerate CRDs.
- **Workload-orchestrator controller** in `velocity-operator`: on
  reconcile, owns (with owner references + GC) a Deployment + Service +
  HPA + ConfigMap + Secret + ingress route per Velocity object. Rolling
  update on spec change; DDL migration sequenced against rollout
  (migrate-then-rollout for additive; breaking ops stay gated by the
  existing annotation). Reconcile-storm damping extended from DB
  provisioning to pod rollout (jittered cascade).
- **`velocity-api` mode switch**: `--single-schema <path>` (data-API,
  schema injected at boot, no registry/router dynamism) vs platform mode
  (keeps informer + `ArcSwap`).
- **platform-API deployment** owning cross-schema search, cross-schema
  query `include[]` + Layer-3 RBAC gate, `/api/platform/*` audit, drift,
  aggregate health, and the admin-read/UI backend (shared with 12c).
- **PgBouncer** (transaction pooling) in front of CNPG; per-pod pool
  ceilings; operator startup verifies pooled connectivity.
- **Operator RBAC** expanded (create/patch/delete Deployments, Services,
  Ingresses/HTTPRoutes, HPAs); generated routes collision-checked.

**Acceptance criteria:**
- Apply a `SchemaDefinition` → operator creates its data-API Deployment +
  Service + route; CRUD works end-to-end against that route.
- Delete the `SchemaDefinition` → its Deployment/Service/route are GC'd.
- Crash/panic in one schema's pod → other schemas' traffic unaffected.
- Cross-schema search and `include[]` joins still work via platform-API.
- Connection count under load stays within CNPG `max_connections`
  (PgBouncer verified).
- Schema spec change → rolling update with zero dropped requests.

### Phase 12b — Anonymous / Auth-Disabled Mode (test-mode) — **shipped (data-plane); integration test pending**

> Landed: `VELOCITY_API_AUTH_MODE` (`config.rs`, default `enforced`), the
> middleware bypass injecting `Identity::anonymous()` before strategy
> resolution (`auth/middleware.rs`), loud signalling (per-process WARN,
> startup banner, `/readyz` banner, `velocity_auth_anonymous_mode` gauge),
> and Helm wiring (`api.auth.mode`). Unit tests cover config parsing and the
> bypass/enforced/non-api-route behaviour. **Pending:** the DB-backed
> integration test (anonymous request → audit row `actor=anonymous` → chain
> verifies) and operator plumbing of the flag onto per-domain data-API pods
> (lands with 12a).


**Goal:** A platform-wide switch to run all services with authentication
bypassed, so functionality can be exercised before auth is re-enabled.
**This is a bypass, not a removal** — identity, audit chain (ADR-005),
and RLS context (ADR-007) stay intact.

**Deliverables:**
- Operator/platform flag `auth.mode: anonymous | enforced` (default
  `enforced`), threaded to every service via config.
- Auth middleware in `velocity-api`: when anonymous, inject a fixed
  `Identity { actor_id: "anonymous", roles: [], attributes: {},
  strategy: "none", issuer: "anonymous" }` and skip verification — no
  code path makes a local decision (ADR-003 discipline).
- `SET LOCAL app.current_user = 'anonymous'` and
  `platform.audit_insert(... 'anonymous' ...)` continue to fire, so the
  chain and RLS context are never undefined.
- Loud signalling: WARN log on every request, a banner in `/readyz`
  output and the UI, and a metric `velocity_auth_anonymous_mode`.
- Inter-service tokens (warm-reader, log-processor) likewise bypassable
  under the same flag.

**Acceptance criteria:**
- With `auth.mode: anonymous`, an unauthenticated request to any data
  endpoint succeeds and produces an audit row with `actor = anonymous`.
- Audit chain verifies clean in anonymous mode.
- Flipping back to `enforced` restores 401 on unauthenticated requests
  with no other change.
- Anonymous mode is impossible to enable silently (banner + metric +
  WARN present whenever active).

### Phase 12c — UI / Information-Architecture Overhaul

**Goal:** Replace the Phase 10 portal's view-centric layout with an
**object-centric tree UI** where every Velocity capability is
configurable. **This supersedes the Phase 10 portal scope.** Served by
platform-API (12a), not the standalone nginx portal, which is retired.

**Prerequisite — admin-read endpoints (currently deferred):** Phase 10's
"Deferred" list records that no admin CRD-read API exists. 12c must add,
on platform-API: list/get for `SchemaDefinition`, `AuthStrategy`,
`RoleBinding`, `ApiKey`, `ArchivePolicy`, `LogFilterPolicy`,
`LogRoutingPolicy`, and the hierarchy (`Organisation`/`Application`/
`Domain`), plus the CRD OpenAPI schema (proxied from kube-apiserver
`/openapi/v3/apis/velocity.sh/v1`) to drive the YAML editor.

**Deliverables:**
- **Left-hand tree panel**: `Org → App → Domain → Object/Version`, plus
  org-level objects (AuthStrategy, RoleBinding, ApiKey, ArchivePolicy,
  Log policies) as sibling branches. Selecting a node opens that object's
  detail UI in the main panel.
- **Every capability configurable from the UI** — schema fields & flags,
  auth strategy, role bindings, archive/log policies, search tier, SLOs,
  time-machine, deployment block (12a) — each as a structured form.
- **"Edit as YAML" action** on every object: opens a **CRD-schema-aware
  YAML editor** (Monaco + the CRD OpenAPI schema for validation,
  completion, and hover docs). Form ↔ YAML round-trip; "Apply" either
  generates a manifest for `velocity apply -f -` (gitops) or POSTs to a
  platform-API write endpoint (decision in 12a/12c).
- Retire `portal/` standalone Dockerfile/nginx + `portal-*.yaml` Helm
  templates (the in-flight working-tree change is folded in here).
- **Dev live-reload mode**: the UI runs under the Vite dev server
  (HMR/live-reload) with its API calls proxied to a running platform-API
  at `velocity.local:8080` (Vite `server.proxy` for `/api`, `/auth`,
  `/version`, `/healthz`, `/readyz`; `velocity.local` resolved via
  `/etc/hosts` or the dev ingress). Same UI bundle builds for the
  served-by-platform-API production path — the only difference is dev
  server + proxy target, configured by env (`VITE_API_BASE` /
  proxy target), no code fork.

**Acceptance criteria:**
- Tree renders the live hierarchy from admin-read endpoints; selecting an
  object opens its editor.
- Creating/editing any supported CRD through a form produces a manifest
  identical (modulo ordering) to hand-written YAML.
- "Edit as YAML" validates against the CRD OpenAPI schema — an invalid
  field is flagged inline before apply.
- All Phase 10 portal views remain reachable within the new IA.

### CLI repositioning (cross-cutting)

The `velocity` CLI is repositioned as the **headless / gitops** surface:
`apply`, `get`, `diff` remain the gitops substrate (CI, `kubectl`-style
flows); interactive configuration moves to the UI (12c). No feature is
removed; the CLI is no longer the primary interactive path. Documented in
`design.md`/`architecture.md` on ADR-011 acceptance.

---

## Milestone Summary

| Phase | Status | Duration | Cumulative | Milestone |
|-------|--------|----------|------------|-----------|
| Pre-Phase | shipped | 1 week | 1 week | ADRs decided |
| 0 | shipped | 2 weeks | 3 weeks | CRDs + Postgres provisioning |
| 1 | shipped | 3 weeks | 6 weeks | SchemaDefinition → working CRUD |
| 2a | shipped | 2 weeks | 8 weeks | JWT + route RBAC |
| 2b | shipped | 2 weeks | 10 weeks | All 7 access control layers |
| 2c | shipped | 1 week | 11 weeks | OIDC + API key + composite |
| 3 | shipped | 2 weeks | 13 weeks | Time machine hot tier |
| 4 | shipped | 1 week | 14 weeks | Time machine warm tier |
| 4.5 | shipped | 1 week | 15 weeks | Operational tooling |
| 5 | shipped | 2 weeks | 17 weeks | Query DSL + 3 search tiers (happy path) |
| 5d | shipped | 1 week | 18 weeks | Search close-out (weights, eager provision, blue-green) |
| 6 | shipped | 2 weeks | 20 weeks | Audit + central logging |
| 7 | shipped | 2 weeks | 22 weeks | Observability |
| 8 | shipped | 2 weeks | 24 weeks | Archive + version lifecycle |
| 9 | shipped | 1 week | 25 weeks | CLI |
| 10 | shipped | 2 weeks | 27 weeks | Portal |
| 11 | planned | 3 weeks | 30 weeks | Production hardening |
| 12a | in progress | 3 weeks | 33 weeks | Per-domain data-API + platform split (core landed; PgBouncer/secret-distribution/KEDA pending) |
| 12b | in progress | 1 week | 34 weeks | Anonymous / auth-disabled test-mode (data-plane shipped) |
| 12c | planned | 3 weeks | 37 weeks | Tree-panel UI overhaul + admin-read endpoints + YAML editor |

**Total: ~30 weeks** (~7 months) for a small team (2-3 engineers). Solo: roughly double.

**Where we are:** through Phase 10 (≈27 of 30 weeks complete). Remaining work is Phase 11 only.

---

## Dependency Order

```
Pre-Phase (ADRs)
    └── Phase 0 (foundation)
            └── Phase 1 (CRUD)
                    └── Phase 2a (JWT + basic RBAC)
                            ├── Phase 2b (advanced access)
                            ├── Phase 3 (time machine hot)
                            │       └── Phase 4 (warm tier)
                            └── Phase 5 (query + search)   [needs 2a only]
                                    └── Phase 5d (search close-out)
                                            ├── Phase 6 (audit + logging)
                                            ├── Phase 7 (observability)
                                            └── Phase 4.5 (ops tooling)
                                            └── Phase 8 (archive)
                                                    └── Phase 9 (CLI)
                                                            └── Phase 10 (portal)
                                                                    └── Phase 11 (hardening)
```

Phases 3, 5, 6, 7 can run in parallel after Phase 2a. Phase 2b can run in parallel with Phase 3.

---

## What's Different from v1

1. **Pre-Phase added** — explicit ADR-decision week before code
2. **Phase 2 split** into 2a (JWT, unblocks rest), 2b (advanced access), 2c (more strategies)
3. **Phase 3 = Time Machine, Phase 5 = Query** — was reversed in v1; review showed query DSL needs time machine context
4. **Phase 4.5 added** — operational tooling, previously not phased
5. **Phase 11 has concrete acceptance criteria** — load test scenarios with numbers
6. **Outbox provisioning starts in Phase 1** — table created even if not used yet, enables Phase 5 search without backfill
7. **Non-superuser role enforced in Phase 0** — was implicit, now explicit gate
8. **arc_swap mentioned from Phase 0** — registry choice from day one
9. **CEL safety constraints in Phase 1** — was treated as detail, now first-class
10. **Total duration: 30 weeks (was 23)** — realistic accounting for the splits, the pre-phase, and the Phase 5 close-out
