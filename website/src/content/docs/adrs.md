---
title: Architecture Decision Records (ADRs)
description: Foundational decisions that shape Velocity — ADR-001 through ADR-010
---

Velocity's design is anchored in 10 Architecture Decision Records. Each ADR is a binding decision; deviations require an ADR update.

## ADR-001: Informer-Per-Replica Architecture

**Status:** Adopted  
**Date:** Phase 1

Every API server replica runs its own kube-rs informer on SchemaDefinition CRDs (etcd watch). Each replica maintains a fresh, in-memory `SchemaRegistry` updated in real-time. No RPC to the operator, no Redis pub/sub, no stale reads.

**Rationale:**
- **Low latency:** Reads are atomic pointer loads (arc_swap), no network hop
- **Availability:** Registry works even if operator is down (operator is for provisioning, not routing)
- **Consistency:** Informer guarantees eventual consistency with etcd; cache misses impossible after reconnect
- **Scalability:** O(replicas) watchers vs. O(1) centralized cache (watchers are cheap; etcd handles fan-out)

**Trade-off:** Each replica uses ~50-100 MB RAM for SchemaRegistry. Acceptable for < 1000 schemas per org.

**Impact:**
- Operator downtime does NOT block API reads
- Schema updates propagate within seconds (informer latency)
- No schema versioning needed at API layer (CRD ResourceVersion handles it)

See CLAUDE.md `SchemaRegistry Implementation (ADR-001, ADR-006)` for code pattern.

---

## ADR-002: Outbox Pattern for CDC

**Status:** Adopted  
**Date:** Phase 4

Every data write that requires indexing (Tier-3 search, future webhooks) uses the transactional outbox pattern:

1. Main table INSERT/UPDATE (e.g., purchase_order_v1)
2. Outbox table INSERT (e.g., purchase_order_v1_outbox) in same transaction
3. CDC worker polls outbox for unpublished rows
4. CDC worker publishes to Typesense, marks published_at
5. Archival sweeps old published rows

**Rationale:**
- **Transactionality:** Main data and index signal in one ACID transaction (no dual-write inconsistency)
- **Idempotency:** CDC worker can retry; duplicates detected by Typesense upsert
- **Auditability:** Outbox lag is observable; stalled CDC is detectable metric
- **Decoupling:** Typesense outage does not block writes (outbox buffers)

**Trade-off:** Extra I/O per write (2 inserts instead of 1). Negligible at < 10K writes/sec.

**Scope:** Tier-3 schemas only. Tier-1 and Tier-2 have no outbox.

**Impact:**
- Typesense indexing is "at most X seconds" behind main table (configurable CDC worker frequency)
- CDC lag alert: `velocity_cdc_lag_records > 1000` indicates worker stall

See CLAUDE.md `Outbox Pattern (ADR-002)` for code pattern.

---

## ADR-003: Authentication Fail-Mode Matrix

**Status:** Adopted  
**Date:** Phase 1

When external dependencies fail (Redis unavailable, JWKS endpoint down, Postgres disconnected), the system defaults to **DENY** (fail-closed). Fail-open is never automatic; it must be explicitly configured and audit-logged.

**Matrix:**

| Dependency | Failure | Default Behavior | Override |
|-----------|---------|------------------|----------|
| Redis (revocation) | Unreachable | 503 REVOCATION_UNAVAILABLE (deny) | `revocation.failOpen: true` (dangerous) |
| JWKS (issuer) | Endpoint down | Cache hit: allow; Cache miss/expire: 401 (deny) | Increase cache TTL (5 min default) |
| Database | Postgres down | 503 SERVICE_UNAVAILABLE (deny) | None |
| Typesense | Indexing unavailable | Writes succeed (outbox buffers); searches degrade to Tier 2 | Automatic fallback |

**Rationale:**
- **Security:** Better to deny a legitimate request than allow an attacker due to failed checks
- **Auditability:** Every fail-mode decision logged in audit trail (`fail_mode` column)
- **Transparency:** Operators can opt-out with explicit configuration and understand the risk

**Trade-off:** User-facing 503s may spike during dependency outages. Uptime SLA depends on dependency health.

**Impact:**
- All authentication requests include `fail_mode` field in audit entry
- Alerts on `velocity_auth_dependency_failures_total` indicate need for incident response

See auth.md `Fail-Mode Matrix (ADR-003)` and hardening.md `Auth Fail-Mode Matrix` for operational details.

---

## ADR-004: Warm-Tier Query Engine (DataFusion)

**Status:** Adopted (Revision 2026-05-18)  
**Date:** Phase 2 (revised Phase 4)

Archive queries (warm tier: S3 Parquet) use Apache DataFusion (embedded Rust SQL engine) in the velocity-warm-reader service.

**Original (Phase 2):** Simple metadata scan + projection.

**Revised (Phase 4):** Full SQL with:
- Predicate pushdown (WHERE filters applied before reading Parquet)
- Aggregations (COUNT, SUM, GROUP BY)
- Sorting and limits
- Columnar pruning (read only selected columns)

**Rationale:**
- **Performance:** Predicate pushdown reduces S3 bytes by 40-60%
- **Simplicity:** SQL is familiar; no custom query DSL
- **Efficiency:** DataFusion is pure Rust; no external process, low latency
- **Compatibility:** Queries match time-machine point-in-time API shape
- **Cost:** Fewer bytes read from S3 = lower egress cost

**Trade-off:** DataFusion lacks some features (no geospatial, limited window functions) — deferred to Phase 10.

**Impact:**
- Archive queries use SQL WHERE/ORDER BY/LIMIT (not custom JSON)
- Response time: 100-500ms typical (S3 + DataFusion overhead)

See archive.md for `Archive Operations` and `Query Archived Records` examples.

---

## ADR-005: Hash-Linked Append-Only Audit Chain

**Status:** Adopted  
**Date:** Phase 1

The audit log is immutable and cryptographically hash-linked: each event includes the SHA256 hash of the previous event. Tampering is detectable.

**Chain structure:**

```
Event 1: SHA256(event_id || timestamp || actor || old || new || "") = abc123...
Event 2: SHA256(event_id || timestamp || actor || old || new || abc123...) = def456...
Event 3: SHA256(event_id || timestamp || actor || old || new || def456...) = ghi789...
```

Any change to Event 2's data (timestamp, actor, values) causes Event 3's hash to break the chain.

**Verification:** `velocity audit verify` recomputes the chain and detects breaks.

**Rationale:**
- **Compliance:** SOC 2, GDPR, HIPAA, PCI-DSS all require tamper-evident audit trails
- **Forensics:** Hash proof that an event was not modified after-the-fact
- **Cryptography:** SHA256 is standard; no custom crypto
- **Simplicity:** Computed at write time in stored procedure; no async verification needed

**Trade-off:** Verification requires reading all events (slow for large records, but typical is 10-100 events per entity).

**Impact:**
- Audit table has `event_hash` and `prev_hash` columns
- Deletion of audit rows is impossible (table is append-only, no superuser privileges)
- All databases and warm-reader queries enforce immutability

See audit.md `Verify Audit Chain Integrity` for verification examples and `Compliance Standards` for regulatory details.

---

## ADR-006: Arc_Swap for Lock-Free Registry Reads

**Status:** Adopted  
**Date:** Phase 1

`SchemaRegistry` uses `arc_swap::ArcSwap<RegistryInner>` for lock-free, concurrent reads. No `RwLock`, no `Mutex`.

**Pattern:**

```rust
struct SchemaRegistry {
    inner: ArcSwap<RegistryInner>,
}

// Write (rare): update
registry.store(Arc::new(new_inner));

// Read (hot path): atomic pointer load
let snapshot = registry.load();
snapshot.resolve(&path)?;
```

**Rationale:**
- **Lock-free:** Reads do not block each other (atomic pointer swap)
- **Performance:** Hot path has zero contention
- **Simplicity:** No lock poisoning; no deadlock risk
- **Fairness:** Writers do not starve readers

**Trade-off:** Readers hold a reference to the old registry snapshot while reading; writes allocate a new ArcSwap. Negligible cost (allocations < 1/min).

**Impact:**
- Schema reads are ~10x faster than RwLock
- API latency does not degrade under high concurrency
- No hand-wavy "lock contention" debugging; performance is predictable

See CLAUDE.md `SchemaRegistry Implementation` for code pattern.

---

## ADR-007: Non-Superuser Postgres Role

**Status:** Adopted  
**Date:** Phase 1

The `velocity_api` role is NON-SUPERUSER with `NOBYPASSRLS=true`. RLS policies are an actual backstop, not just app-layer logic.

**Verification:**

```sql
SELECT rolname, rolinherit, rolcanlogin, rolbypassrls
FROM pg_roles
WHERE rolname = 'velocity_api';

-- Expected:
-- rolname    rolinherit  rolcanlogin  rolbypassrls
-- velocity_api  t       t            f  ← Must be false
```

If `rolbypassrls = true`, RLS is bypassed and security is compromised.

**Constraints applied to velocity_api:**
- No superuser grant
- No CREATE ROLE
- No ALTER DATABASE
- No TRUNCATE (DELETE only)
- SELECT/INSERT/UPDATE/DELETE on schema-specific tables only

**Rationale:**
- **Defense in depth:** Even if app code has a bug, RLS prevents unauthorized reads
- **Compliance:** SOC 2, HIPAA require separation of duty and access controls
- **Simplicity:** Postgres enforces the policy; no trust in app logic
- **Auditability:** `pg_audit` can log all RLS violations at database level

**Trade-off:** App cannot bypass RLS (e.g., internal admin operations must use a separate role).

**Impact:**
- Every transaction must call `SET LOCAL ROLE {domain_role}` (app-layer context switching)
- RLS filters are applied even if app code forgets to include them
- Operators must verify role at startup; deployment fails if misconfigured

See installation.md `Critical: ADR-007 Verification` and hardening.md `Database Role Verification (ADR-007)` for operational details.

---

## ADR-008: CEL for Safe Expressions

**Status:** Adopted  
**Date:** Phase 7

CEL (Common Expression Language) is used for ABAC, validation rules, and archive triggers. Execution is bounded: 10 KB max, 10 depth, 10 ms timeout.

**Constraints:**

```
max_expression_size: 10 KB
max_execution_depth: 10
max_execution_duration: 10 ms
forbidden_functions: [matches with unbounded regex]
```

**Examples:**

```yaml
# Valid CEL
condition: "actor.department in ['procurement', 'finance']"
condition: "self.amount > 1000 && self.status == 'approved'"
condition: "self.created_at > now() - duration('7776000s')"

# Invalid CEL (too slow)
condition: "self.description.matches('.*')"  # Unbounded regex

# Invalid CEL (too long)
condition: "...5000 character expression..."

# Invalid CEL (too deep)
condition: "a || b || c || d || ... (>10 OR chains)"
```

**Rationale:**
- **Denial-of-service prevention:** Bounded execution prevents CPU exhaustion
- **Consistency:** Same CEL used everywhere; no custom expression language per feature
- **Safety:** No file I/O, network calls, or external dependencies in CEL
- **Debuggability:** Timeout vs. parse error is explicit

**Trade-off:** Complex business logic must use triggers or async reconciliation, not CEL.

**Impact:**
- Archive triggers and validation rules use CEL
- `velocity schema validate` checks expression constraints at apply time
- Webhook server never runs untrusted CEL; it runs trusted operator-defined CEL only

See features/schema-definition.md `CEL Constraints` and hardening.md `Input Validation` for examples.

---

## ADR-009: Cursor-Based Pagination (Deferred)

**Status:** Planned for Phase 11  
**Date:** TBD

For scalability, large result sets (> 1000 rows) use cursor-based pagination instead of offset/limit.

**Current behavior:** offset/limit, max 1000 rows per request.

**Planned behavior:** `GET /api/{path}?cursor={token}&limit=100` returns `next_cursor` in response.

**Rationale:**
- **Performance:** Cursor avoids O(N) row skip overhead
- **Consistency:** Cursor snapshots the result set; offset-based pagination has gaps/dupes on concurrent writes
- **Predictability:** Cursor response time is constant; offset grows with skip distance

**Trade-off:** Client must handle pagination differently (not just loop with limit += 100).

**Impact:** When implemented, offset-based pagination will be deprecated but not removed.

See API reference for current pagination behavior (offset/limit).

---

## ADR-010: Multi-Tenant Isolation (Deferred)

**Status:** Planned for Phase 11  
**Date:** TBD

Hard tenant boundaries for true SaaS multi-tenancy. Org-level isolation enforced in schema definitions.

**Planned constraints:**
- No cross-org schema references
- Audit log includes org context
- Role binding is org-scoped
- Tenant quotas (schemas per org, records per schema)
- Separate admin personas per tenant

**Current behavior:** Single org per deployment. Org is set at install time (Helm values).

**Future behavior:** Multiple orgs in one Velocity cluster with hard boundaries.

**Rationale:**
- **Security:** Accidental data leakage between customers is impossible
- **Compliance:** HIPAA, PCI-DSS require data segregation
- **Cost:** One Postgres instance per cluster (not per org)
- **Simplicity:** RLS policies are org-scoped only

**Trade-off:** Cross-org queries will never be supported (by design).

**Impact:** When implemented, Helm deployment will accept `multiTenant: true` flag. Single-tenant deployments remain unchanged.

---

## ADRs Awaiting Finalization

None at this time. ADRs 001–010 are stable.

**Future ADRs may address:**
- Sharding strategy (Phase 11)
- Webhook delivery guarantees (Phase 10)
- Search analytics backend (Phase 10)
- Disaster recovery RTO/RPO targets

---

## How to Reference an ADR

In code comments and design docs, cite by number:

```
# Valid
# ADR-007 requires non-superuser role for RLS

# ADR-003 fail-mode matrix applies here

# Invalid
# The non-superuser thing
# Security reasons for role separation
```

## Updating an ADR

To update a decision:

1. Create a new ADR with revision date (e.g., "ADR-004 Revision 2026-05-18")
2. Explain change from previous decision
3. Update CLAUDE.md `Required Reading` section
4. Update all code comments and docs
5. File a PR with `[ADR-XXX]` tag in commit message

---

## Compliance Mapping

| Standard | ADRs |
|----------|------|
| SOC 2 Type II | ADR-005 (audit), ADR-007 (RLS, access controls) |
| GDPR | ADR-005 (audit trail), ADR-007 (access controls), ADR-010 (data segregation) |
| HIPAA | ADR-005 (tamper detection), ADR-007 (RLS), ADR-010 (hard boundaries) |
| PCI-DSS | ADR-005 (immutable audit), ADR-007 (role separation) |
| SOX | ADR-005 (audit chain), ADR-007 (separation of duty) |

