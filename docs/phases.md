# Velocity — Phase-wise Implementation Plan (v2)

> Each phase produces a working, testable increment.
> v2 changes from v1: Phase 2 split into 2a/2b/2c. Phase 4 (Time Machine) moved before Phase 5 (Query). New Phase 4.5 (Operational tooling). Concrete acceptance criteria for Phase 10.

---

## Pre-Phase — Architectural Decisions (1 week)

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

## Phase 0 — Foundation (2 weeks)

**Goal:** Repository skeleton, CRD types, Postgres provisioning wired end-to-end. Proves operator ↔ Postgres works with non-superuser role (ADR-007).

### Deliverables

**Repository structure:**

```
velocity/
├── crates/
│   ├── velocity-types/
│   ├── velocity-operator/
│   ├── velocity-api/
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

## Phase 1 — Core CRUD (3 weeks)

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

## Phase 2a — JWT + Basic Auth (2 weeks)

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

## Phase 2b — Advanced Access Control (2 weeks)

**Goal:** Field filtering, row filtering, ABAC (CEL), Postgres RLS.

### Deliverables

**Layer 2: ABAC (CEL):**
- Compile CEL at schema load time
- Evaluate with 10ms timeout (ADR / S2)
- Deny on timeout or error

**Layer 3: Cross-schema RBAC (review fix):**
- Before adding joins/includes, verify identity has read on target schema
- Test case: actor with `procurement-reader` cannot include `supplier` data
- **Status: deferred to Phase 5.** The query engine does not yet parse `include[]` or build joins, so there is no code path that could leak cross-schema data. Implementing Layer 3 now would be a stub against an unused entry point. Phase 5's PR that adds include semantics MUST land the cross-schema RBAC check in the same commit (default-deny: actor without `read` on the referenced schema → 403 before SQL is built). The query module's `build_list` carries a TODO comment at the future include-parse site to ensure that PR cannot land without this gate.

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

## Phase 2c — Additional Auth Strategies (1 week)

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

## Phase 3 — Time Machine (2 weeks)

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

## Phase 4 — Time Machine Tiering (1 week)

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

## Phase 4.5 — Operational Tooling (1 week)

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

## Phase 5 — Query Engine & Search (2 weeks)

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

---

## Phase 6 — Audit & Central Logging (2 weeks)

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

## Phase 7 — Observability (2 weeks)

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

## Phase 8 — Archive & Lifecycle (2 weeks)

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

---

## Phase 9 — CLI (1 week)

**Goal:** `velocity` binary distributed as single static binary.

### Deliverables

**velocity-cli crate** with full command set:
- apply, get, describe, delete, diff
- logs, history, query, export
- restore, grant, revoke
- create api-key, revoke api-key
- archive, unarchive, approve
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

## Phase 10 — Admin Portal (2 weeks)

**Goal:** React SPA for managing Velocity objects. Visual editor with live YAML preview.

### Deliverables

**velocity-portal (React + Vite):**
- Dark theme, monospace, amber accent
- Three-panel layout

**Views:**
- Overview (counts, health, recent events)
- Hierarchy browser
- Schema list + detail
- AuthStrategy list
- RoleBinding management + grant form
- ApiKey management + create form
- Central logging (stream, pipeline, stats)
- LogFilterPolicy + rule editor
- LogRoutingPolicy + YAML
- Audit log with filters
- Health per schema
- Metrics per schema

**Visual object editor:**
- Object type picker
- Form-based, no raw YAML
- Live YAML preview
- Copy YAML / Copy command / Apply
- Pre-populate from existing objects

**Deployment:**
- Single Docker image (nginx + static build)
- Helm chart
- OIDC login via Velocity's OIDC server

---

## Phase 11 — Hardening & Production Readiness (3 weeks)

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

## Milestone Summary

| Phase | Duration | Cumulative | Milestone |
|-------|----------|------------|-----------|
| Pre-Phase | 1 week | 1 week | ADRs decided |
| 0 | 2 weeks | 3 weeks | CRDs + Postgres provisioning |
| 1 | 3 weeks | 6 weeks | SchemaDefinition → working CRUD |
| 2a | 2 weeks | 8 weeks | JWT + route RBAC |
| 2b | 2 weeks | 10 weeks | All 7 access control layers |
| 2c | 1 week | 11 weeks | OIDC + API key + composite |
| 3 | 2 weeks | 13 weeks | Time machine hot tier |
| 4 | 1 week | 14 weeks | Time machine warm tier |
| 4.5 | 1 week | 15 weeks | Operational tooling |
| 5 | 2 weeks | 17 weeks | Query DSL + 3 search tiers |
| 6 | 2 weeks | 19 weeks | Audit + central logging |
| 7 | 2 weeks | 21 weeks | Observability |
| 8 | 2 weeks | 23 weeks | Archive + version lifecycle |
| 9 | 1 week | 24 weeks | CLI |
| 10 | 2 weeks | 26 weeks | Portal |
| 11 | 3 weeks | 29 weeks | Production hardening |

**Total: ~29 weeks** (~7 months) for a small team (2-3 engineers). Solo: roughly double.

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
10. **Total duration: 29 weeks (was 23)** — realistic accounting for the splits and the pre-phase
