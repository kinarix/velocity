---
title: Changelog
description: Velocity release history — Phase 1 through Phase 9
---

Velocity follows a phased delivery model. Each phase adds capabilities and is production-ready on completion.

## Phase 1: Core Platform (Completed)

**Release:** Q1 2026

Foundational schema-driven backend. Single-tenant, single-org testing.

- **SchemaDefinition CRD:** CRUD, validation, auth, versioning
- **REST API:** POST/GET/PATCH/DELETE on auto-generated routes
- **Postgres provisioning:** Tables, indexes, RLS policies, roles
- **JWT authentication:** JWKS cache, claim mapping, revocation via Redis
- **Authorization:** Layer 1 (RBAC) + Layer 7 (RLS)
- **Audit log:** Append-only, hash-linked, tamper detection
- **Validating webhook:** Schema validation before apply
- **CLI:** schema apply, auth login, audit list
- **Observability:** Prometheus metrics, OpenTelemetry traces, JSON logs

**Key ADRs:** ADR-001 (informer-per-replica), ADR-003 (fail-closed auth), ADR-005 (hash-linked audit), ADR-007 (non-superuser role)

## Phase 2: Time Machine (Completed)

**Release:** Q2 2026

Point-in-time query and restore.

- **History tables:** Atomic transaction with main table update
- **Hot tier:** Postgres history partitioned by month
- **Warm tier:** S3 Parquet export (90d+ threshold)
- **Point-in-time query:** Reconstruct record at any timestamp
- **Diff:** Compare state between two timestamps
- **Restore:** Write a new RESTORE event (immutable history)
- **Snapshots:** Export all records at a specific time for compliance
- **CLI:** history list, history at, history diff, restore

**Key features:**
- Automatic partition archival to S3 after 90 days
- RLS enforced on historical queries (you see only what you can read now)
- Sensitive field redaction in history
- Replay (Server-Sent Events stream of events)

## Phase 3: Search Tiers 1 & 2 (Completed)

**Release:** Q2 2026

Trigram substring matching and Postgres full-text search.

- **Tier 1 (Trigram):** < 5ms, default on all strings, no relevance
- **Tier 2 (Postgres FTS):** 50-100ms, ranked results, stemming, phrase queries, 13+ languages
- **Field configuration:** `searchable: true/false`, language-specific stemming rules
- **Query endpoint:** POST /{path}/query with WHERE, ORDER BY, LIMIT
- **Query builder:** Invariant enforcement (only filterable/sortable fields)
- **Cross-schema search:** GET /{org}/search across all schemas with RLS enforcement
- **CLI:** schema apply with search tier configuration

**Language support:** English, German, French, Spanish, Portuguese, Italian, Dutch, Swedish, Norwegian, Danish, Russian, Chinese, Japanese.

## Phase 4: Real-Time Search & CDC (Partial — Tier 3 and Outbox Complete)

**Release:** Q2/Q3 2026 (Tier 3 only; Tier 2 → Tier 3 migration deferred)

Real-time, typo-tolerant, faceted search via Typesense.

- **Tier 3 (Typesense):** < 20ms, typo tolerance, faceting, prefix search, real-time indexing
- **Outbox CDC pattern:** Main table write + outbox write in same transaction (ADR-002)
- **CDC worker:** Reads outbox, publishes to Typesense, marks published_at
- **Field indexing:** searchable (indexed), facet (discrete values), neither (stored but not indexed)
- **Blue-green reindex:** Zero-downtime index swap when schema changes
- **Cost:** $50-100/month Typesense Cloud or self-hosted
- **CLI:** schema apply with Tier 3 configuration

**Deferred:**
- Tier 2 → Tier 3 live migration (will be Phase 4 revision)
- Warm-tier FTS (deferred to Phase 4 revision)

## Phase 5: Cross-Schema Joins (Deferred — Phase 5 TBD 2026)

Advanced querying across related schemas.

**Planned:**
- JOIN syntax (one level)
- Cross-schema RBAC enforcement (Layer 3 access control)
- Foreign key constraints
- Referential integrity via triggers

**Status:** Waiting for Q3 2026 for capacity planning.

## Phase 6: API Keys & Multi-Auth (Completed)

**Release:** Q2 2026

API key authentication, OIDC, composite auth strategies.

- **API keys:** SHA256 hashed, IP allowlist, TTL expiring, revocable
- **OIDC:** Browser-based authorization code flow, session cookies, PKCE
- **Composite strategies:** Try multiple auth methods in order
- **Claim mapping:** Extract JWT claims to Identity (role, attributes, store_id, region)
- **Transforms:** identity, prefix_strip, split, uppercase, lookup, regex_extract, static_append
- **RoleBinding:** Bind actors to roles with expiry, scope attributes, audit trail
- **CLI:** auth login, api-key create/list/revoke, grant, revoke

**Key features:**
- Revocation immediate via Redis broadcast
- Token TTL default 15-60 minutes
- IP restriction per key
- Audit every auth decision (outcome, fail_mode, strategy)

## Phase 7: Authorization Layers 2–6 (Completed)

**Release:** Q3 2026

Full access control stack: ABAC, row filtering, field filtering, masking.

- **Layer 2 (ABAC):** CEL expressions at request time (department, tenure, manager status)
- **Layer 3 (Cross-schema RBAC):** Verify access before JOIN (deferred until Phase 5)
- **Layer 4 (Row filtering):** Scope rows by actor attribute (region, store_id, team)
- **Layer 5 (Field filtering):** Hide sensitive fields on read, reject writes to restricted fields
- **Layer 6 (Masking):** Partial (****1234), full (****), redaction per field
- **Audit masking:** Sensitive fields redacted in audit entries and history
- **CEL safety:** 10 KB max, 10 execution depth, 10ms timeout
- **Schema policy:** requireReason for mutations, breaking change approval
- **CLI:** grant, revoke, role list

**Constraints enforced:**
- All 7 layers applied per request
- Fail-closed on any denial
- Audit trail records which layers triggered denial

## Phase 8: Archive & Lifecycle (Completed)

**Release:** Q3 2026

Automatic cold storage with tiered lifecycle.

### Slice 1–5: Foundation (Completed)

- **ArchivePolicy CRD:** Trigger types (age, size), S3 destination
- **Archive worker:** Batch export to Parquet, scheduled runs
- **Warm tier:** S3 queryable via warm-reader (DataFusion)
- **PurgeRequest:** Permanent deletion workflow
- **RLS on warm:** Access control enforced at query time
- **Cost estimation:** ~$60-100/year for 100K records/month over 5 years

### Slice 6: Hot Purge (Completed Q3 2026)

- **Purge immediate:** Via hot purge (delete from main table before archival)
- **Purge scheduled:** Via lifecycle job (delete from archive when aged out)
- **Audit:** Every purge logged with reason
- **Approval flow:** Manual approval for retention policy compliance

### Slice 7: PurgeRequest Controller (Completed Q3 2026)

- **CRD:** PurgeRequest with criteria, estimatedRecords, requiresApproval
- **Operator reconciliation:** Validates policy, estimates record count, holds in Pending
- **Approval:** velocity approve command, updates status to Approved
- **Execution:** Archive worker deletes records on next run
- **Immutable audit trail:** Purge reason, approver, timestamp in audit log

### Slice 8: Field/TableSize Triggers (Completed)

- **Trigger type: age:** Archive records > N days old
- **Trigger type: size:** Archive when table > N GB
- **Both implemented:** Checked on schedule, applied independently
- **Field-level trigger:** Deferred to Phase 10 (CEL conditions per field)

### Slice 9: Archive API (Completed)

- **GET {id}/archive:** Fetch archived version by ID
- **POST /archive/query:** Query Parquet files via warm-reader (DataFusion SQL)
- **POST /unarchive:** Restore record to hot tier
- **RLS enforced:** Only see archived records you can read now
- **Performance:** 100-500ms typical (S3 + DataFusion)

### Slice 10: S3 Parquet Destination (Completed)

- **Format:** Columnar Parquet (40-60% compression vs JSON)
- **Partitioning:** s3://bucket/{org}/{app}/{domain}/{object}_v{version}/{month}
- **Schema inference:** From SchemaDefinition field types
- **Compatibility:** Readable by Spark, Presto, BigQuery, Athena
- **Lifecycle:** S3 lifecycle rules transition to Glacier after 1 year

**Key features:**
- Idempotent batch writer (resume on failure)
- Atomic commit per batch
- Metrics: archive_records_total, archive_duration_seconds, archive_s3_bytes
- Monitoring: CDC lag (unpublished outbox), stalled archives

## Phase 9: Observability & Runbooks (Completed)

**Release:** Q3 2026

Comprehensive metrics, traces, SLOs, and operational runbooks.

### Slice 1–4: Observability Foundation (Completed)

- **Prometheus metrics:** 30+ metrics with bounded label cardinality
- **OpenTelemetry traces:** Trace propagation across service boundaries (warm-reader RPC)
- **Structured logging:** JSON-only, no full payloads, sensitive field redaction
- **SLO dashboards:** Grafana with recording rules (99.9% API availability)
- **Alerting:** PrometheusRule manifests (critical for tampering, warning for latency regression)

### Slice 5: Runbooks (Completed)

- **postgres-failover:** CNPG cluster switchover, DNS update verification
- **restore-from-backup:** pg_dump restore from S3, schema re-sync, data verification
- **rotate-api-key:** Create new key, migrate deployments, revoke old key
- **quarantine-drifted-schema:** Understand drift, manual schema re-apply, operator restart
- **oncall-cheatsheet:** What to grab in incident (logs, metrics, audit, trace ID)

### Slice 6: Archive Worker Observability (Completed)

- **Metrics:** archive_runs_total, archive_records_total, archive_duration_seconds
- **Alerts:** ArchiveStalled (not run in 24h), OutboxLag (> 10K unpublished)
- **CDC lag monitoring:** velocity_cdc_lag_records per schema
- **Dashboard:** Archive health, throughput, errors over time

## Phase 10: Advanced Features (Deferred — Q4 2026 TBD)

Not yet started.

**Planned:**
- **CEL field/custom triggers:** Condition-based archive eligibility
- **Typed Arrow columns:** Preserve schema types in Parquet for efficient queries
- **Warm-tier joins:** Multi-table queries on archived data
- **Orphan sweep:** Purge archived records whose source was deleted
- **Geo search:** Distance queries on Typesense (Phase 10)
- **Synonym management:** Custom synonym maps per schema (Phase 10)
- **Search analytics:** Popular queries, click-through rates (Phase 10)
- **Cold-tier restore:** Move Glacier data back to hot on demand (Phase 10)
- **Selective restore:** Restore some fields only (Phase 10)
- **Warm-tier cross-dataset joins:** Multi-schema warm queries (Phase 4 revision)

## Phase 11: Sharding & Multi-Tenant Scale (Deferred — 2027 TBD)

**Planned:**
- **Sharded archive workers:** Parallel batch processing (one worker per shard)
- **Database sharding:** Split large schemas across multiple Postgres instances
- **Multi-tenant isolation:** Hard tenant boundaries (per ADR-010 when finalized)
- **Tenant quotas:** Rate limits per org/app/domain
- **Cross-tenant audit:** Global audit dashboard with access filtering

## Breaking Changes

Velocity 0.1.0 → 1.0.0 will include breaking changes:

- SchemaDefinition v1 → v2 (field type system may expand)
- Index naming conventions (may change for maintainability)
- Audit log schema (unlikely, but possible)

**Promise:** Minor version bumps (0.1 → 0.2) are backward compatible. Major bumps (0.x → 1.0) include migration guides.

## Security Fixes

### Q2 2026

- Fix: Operator RBAC did not include verb:watch (informer requires watch access) — resolved in Phase 1.2
- Fix: Redis revocation cache TTL too long (24h → 5m) — resolved in Phase 6.1

### Q3 2026

- No critical security issues reported.

## Performance Improvements

### Phase 3

- Postgres FTS query index tuning: 50-100ms → 20-50ms p99 latency
- GIN index on TSVECTOR columns reduced memory footprint 40%

### Phase 8

- Archive worker batch size tuning: 10K records → configurable, 5K-50K optimized per schema
- Warm-reader DataFusion predicate pushdown: S3 object filtering pre-query (40% fewer bytes scanned)

### Phase 9

- Metrics cardinality audit: Removed 12 high-cardinality label combinations
- Trace sampling: 100% → 10% default (configurable), reduced exporter load 90%

## Known Limitations

- **Cross-schema joins:** Not yet implemented (Phase 5 TBD)
- **CEL field triggers:** Deferred to Phase 10
- **Warm-tier FTS:** Deferred to Phase 4 revision
- **Cold-tier restore:** Glacier files immutable until Phase 10
- **Selective restore:** All-or-nothing only until Phase 10
- **Geo search:** Deferred to Phase 10
- **Search reindex:** Manual via annotation until Phase 11
- **Synonym management:** Deferred to Phase 10
- **Search analytics:** Deferred to Phase 10

## Upgrade Path

### 0.1.0 → 0.1.1

Patch release (bug fixes only). No schema changes. Helm upgrade:

```bash
helm upgrade velocity velocity/velocity \
  --namespace velocity-system \
  --values values.yaml
```

### 0.1.0 → 0.2.0

Minor release. Backward compatible. May add new schema fields (optional). Migration:

1. Upgrade Helm chart
2. Restart API pods (no data migration needed)
3. Run `velocity schema apply` on existing schemas (picks up new features)

### 0.x.0 → 1.0.0

Major release (future). Includes breaking changes. Migration guide will be published.

