# Velocity — Architectural Decision Records

> Records of significant architectural decisions made during design.
> Each ADR captures the context, options considered, decision, and rationale.
> ADRs are immutable once accepted — superseded ADRs reference their replacement.

---

## Status Legend

- **Proposed** — under discussion
- **Accepted** — decided, implementation should follow
- **Superseded** — replaced by a later ADR
- **Deprecated** — no longer applies, no replacement needed

---

## ADR-001: SchemaRegistry Consistency Model

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

The `SchemaRegistry` is an in-memory representation of all `ResolvedSchema` objects, read on every API request. The API server runs as N replicas (up to 30). The operator runs as 2 replicas (HA via leader election). The registry must be consistent across all API replicas — if one replica enforces stale auth rules, the security posture is unpredictable.

Three options considered:

| Option | Mechanism | Pros | Cons |
|--------|-----------|------|------|
| A. Each API replica watches CRDs | Kube informer per replica | No operator → API gap, etcd is source of truth, eventual consistency well-bounded | Slightly more kube-apiserver load |
| B. Operator → Postgres → API LISTEN/NOTIFY | Postgres mirror of CRDs | Decouples API from k8s API server | Two sources of truth, NOTIFY not durable, race conditions during reload |
| C. Operator → Redis pub/sub → API | Low latency | Fast propagation | Redis is not durable, missed messages on startup → stale, needs periodic full reload as safety net |

**Decision:** Option A — each API server replica runs its own kube informer.

**Rationale:**
- etcd is the only source of truth, eliminating an entire class of consistency bugs
- Standard pattern in Kubernetes ecosystem (matches how Istio, Argo CD, KEDA work)
- Convergence time is well-bounded (~100ms typical, 1s worst-case)
- No new infrastructure required
- Survives operator restarts, etcd connectivity blips, etc.

**Consequences:**
- API server has a hard dependency on kube-apiserver reachability at startup
- Modest additional load on kube-apiserver (informer watches are cheap)
- Each replica converges independently — brief windows where replicas disagree (acceptable for our consistency model)

**Implementation note:**
Build the registry from a `kube::runtime::watcher` stream. Use `arc_swap::ArcSwap` (see ADR-006) for lock-free reads. Block API server startup until first full informer sync completes (readiness probe gates traffic).

---

## ADR-002: CDC Mechanism for Search Index Sync

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

Tier-3 search requires propagating every Postgres write to Typesense. The original design used `LISTEN/NOTIFY` — which is **fire-and-forget**. Postgres does not retain unsent notifications. A CDC worker restart or network blip causes permanent index drift with no detection mechanism.

Three options considered:

| Option | Mechanism | Pros | Cons |
|--------|-----------|------|------|
| A. WAL-based CDC | Debezium / pg_replication_slot | Industry standard, durable, captures every change | Operational complexity, extra component, Debezium runs on JVM |
| B. Outbox pattern | Trigger writes to outbox table in same transaction | Durable by design, simple, no new infrastructure | Slightly higher write cost (extra INSERT per mutation) |
| C. LISTEN/NOTIFY + reconciliation | Original design + periodic full reindex | Simple | Silent drift between reconciliations, expensive to reconcile at scale |

**Decision:** Option B — outbox pattern.

**Rationale:**
- The outbox INSERT is in the same transaction as the data write — atomicity guaranteed
- CDC worker reads the outbox table directly — simple Postgres query, no special protocol
- Worker tracks position via a marker (UPDATE `published_at`) — durable across restarts
- No new components, no JVM, fits the schema-driven operator model
- Cost: one extra INSERT per mutation (≈10% write overhead, acceptable for Tier-3 schemas)

**Consequences:**
- Operator must provision an outbox table for every Tier-3 schema
- Outbox table needs periodic vacuum/cleanup (published rows older than 24h deleted)
- Recovery from worker crash: simply restart, worker picks up unpublished rows

**Implementation note:**
Outbox table generated alongside main table. Index on `(published_at) WHERE published_at IS NULL`. Worker uses `SELECT ... FOR UPDATE SKIP LOCKED` for concurrent workers without contention.

---

## ADR-003: Failure Mode Defaults Across Auth and Access Dependencies

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

The architecture has many components that can fail — JWKS endpoint, Redis (revocation), Postgres (RBAC), SchemaRegistry, CEL evaluator. Without a single, explicit failure-mode policy, different code paths make inconsistent decisions and the security posture becomes unpredictable. A subtle case: Redis goes down, half the requests deny (correct) and half allow (incorrect) depending on which code path was used.

**Decision:** Apply this matrix to all auth and access control dependencies:

| Dependency | Default fail-mode | Override available | Reasoning |
|-----------|-------------------|--------------------|-----------|
| JWKS endpoint | Use cached keys (continue) | None | Cached keys are cryptographically signed; using them when source is unreachable is safe |
| JWKS cache empty (startup) | Deny all requests | None | We don't know what's valid; refuse rather than risk wrong decision |
| Redis (revocation) | Deny | `failOpen: true` per AuthStrategy | Default fail-closed; high-availability public APIs may opt out |
| Postgres (RBAC lookup) | Deny | None | Can't determine if actor has access; refuse |
| SchemaRegistry empty | Return 503 with Retry-After | None | Platform isn't ready; clients should retry |
| CEL evaluator error | Deny | None | Broken rule = unsafe state; bail |
| Hook target unreachable | Continue, queue for retry | Per-hook config | Non-security path; eventual delivery is acceptable |
| Typesense unreachable | Fall back to Tier-2 (Postgres FTS) | None | Search degrades but works |
| Kafka unreachable | Queue locally for up to 5 minutes | None | Hooks/events eventually delivered |

**Rationale:**
- "Deny by default for anything touching auth/access" — the security industry default
- Non-security paths get continuity to maximize availability
- Override available only where business need clearly outweighs security risk (e.g., a public API where Redis flakiness shouldn't take down read traffic)

**Consequences:**
- Operators must monitor Redis availability — its outage now causes auth failures by default
- AuthStrategy gains `failOpen` field for explicit opt-out
- Every dependency check site in code must reference this matrix and not make local decisions

**Implementation note:**
Create a `FailMode` enum and corresponding helper functions. Every dependency check uses the helper, never makes the decision locally. Audit log records the fail-mode applied to each request for forensics.

---

## ADR-004: Time Machine Tiered Storage

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

Without tiering, the event log for a 500M-record schema with 10 average mutations per record reaches 5 billion rows (~2.5TB). Postgres performance degrades, costs spiral, and teams disable time machine for high-traffic schemas — defeating the audit value.

**Decision:** Three-tier storage with automatic migration.

```
Tier        Backend      Retention    Query latency   Use case
─────────────────────────────────────────────────────────────────────────
Hot         Postgres     90 days      < 50ms          Recent history, restore
Warm        S3 Parquet   5 years      2-10s           Investigations, audits
Cold        S3 Glacier   7 years      5-12 hours      Compliance retrieval
```

Migration is operator-driven:
1. Hot tier is `platform.event_log` partitioned by month
2. Nightly job: export oldest hot partition to Parquet on S3, then drop partition
3. S3 lifecycle policy: move Parquet objects older than 1 year to Glacier

**Rationale:**
- Hot tier is bounded — Postgres only ever holds 90 days of events per schema
- Warm tier handles 99% of audit/investigation needs (last 5 years)
- Cold tier handles regulatory retention without paying hot storage cost
- Costs become predictable and linear, not exponential

**Consequences:**
- Point-in-time queries crossing tier boundaries need transparent routing
- Cold tier queries are async — return a job ID, deliver via webhook when ready
- The time machine API must handle "Retrieval scheduled" responses for cold tier
- Per-schema configuration via `timeMachine.storage.{hot,warm,cold}.retention`

**Implementation note:**
Hot partitioning uses Postgres native partitioning (PARTITION BY RANGE on `occurred_at`, monthly). Warm queries originally specified DuckDB to query Parquet on S3; see the revision below for the current choice. Cold tier retrieval uses Glacier Initiate Retrieval API; completion notification via SNS → Kafka → user callback.

**Revised 2026-05-18 — warm-tier query engine and process boundary:**

Two changes from the original note.

*Engine.* The original comparison was DuckDB to Athena only; DataFusion was not considered. After evaluation, the warm-tier reader uses **DataFusion** on top of `parquet` + `arrow` + `object_store`. The query path today is the narrow DataFrame shape (filter + sort + limit + project); the SQL surface comes for free when we want it.

Reasoning:
- The API's warm-read query shape is fixed and narrow today — range scan on `occurred_at`, filter on `(schema_org, entity_id)`, project a handful of columns, fold to state via existing JSON Patch logic. DataFusion expresses that as a few `DataFrame::filter` calls; it does NOT obligate us to expose SQL.
- DataFusion gives us Parquet predicate pushdown via row-group statistics automatically — no hand-rolled `cmp::eq` + `filter_record_batch` we'd otherwise have to maintain per column.
- DataFusion is pure-Rust, builds in seconds, shares the same `arrow` + `parquet` + `object_store` versions the operator already pins. Zero new core deps; one extra crate (the query planner) on top of stack we already have.
- DataFusion's `ListingTable` / `read_parquet` handles multi-file scans natively, so we don't write a per-file orchestrator on the reader side.
- DuckDB's strengths — mature SQL optimizer, extension ecosystem, REPL — accrue to humans doing ad-hoc analytics, not to the API path. Those use cases can run the `duckdb` CLI against the same S3 Parquet objects without it being a dependency of the API binary.
- When SQL-over-warm becomes a concrete API requirement (admin console, customer-facing audit explorer), it lands as `ctx.sql("...")` against the same `SessionContext` we already maintain. No rewrite of the simple reader.

Operationally, ops/analysts may still use the `duckdb` CLI ad-hoc against warm-tier S3 objects — it is a human tool, not a service dependency.

*Process boundary.* The warm-tier reader runs in its own service, **`velocity-warm-reader`**, not in-process inside `velocity-api`. Rationale:

- Resource profile differs — warm scans are IO and memory-heavy (row-group decode into Arrow), API hot path is QPS-heavy. Mixing them puts warm scans on the API's memory budget.
- Failure isolation — an S3 degradation contaminates the warm service's tail, not the API's hot-path tail. The hot path stays clean even when warm tier is sick.
- Future engine growth (DataFusion, Parquet metadata cache, parallelism) does not bloat every API replica.
- Reads and writes of warm tier are separate concerns: write lives in `velocity-archive-worker` (export), read lives in `velocity-warm-reader` (query). Co-locating would conflate two failure domains under one binary name.

The API talks to `velocity-warm-reader` over **HTTP** (axum server, reqwest client — both already workspace dependencies, no new transport framework). This is the codebase's first internal service-to-service RPC; the pattern established here becomes the template for future internal services:

- Service-token auth (`Authorization: Bearer <token>`), token in a k8s Secret.
- OTel trace propagation via the `traceparent` header.
- Fail-closed defaults consistent with ADR-003 — when warm-reader is unavailable, the API returns 503 to the caller, never silently degrades to empty results.
- 15 s default request timeout; configurable per call-site.
- Structured error envelope: `{ "code": "...", "message": "...", "request_id": "..." }` mirroring the API's existing error shape.

When the trigger arises (mTLS, mutual SPIFFE, multi-region request routing), it lands as a follow-up ADR rather than a Phase 4 detour.

---

## ADR-005: Audit Log Chain Integrity Mechanism

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

The audit log is chain-hashed for tamper evidence — each row includes the SHA256 hash of the previous row. With N async writers (one per API replica × multiple goroutines), there is no natural ordering, and concurrent writes producing the same `prev_hash` break the chain.

Three options considered:

| Option | Mechanism | Pros | Cons |
|--------|-----------|------|------|
| A. Single writer per replica | Channel + dedicated goroutine | Simple, lock-free | Per-replica chain, not global; complicates verification |
| B. DB-level stored procedure | `velocity_audit_insert()` with row lock on latest row | Single global chain, atomic | Higher write latency (lock contention) |
| C. Per-replica chains | Each replica maintains its own chain | No contention | Verification queries per replica; less useful audit semantics |

**Decision:** Option B — DB-level stored procedure with row lock.

**Rationale:**
- Audit log is async (writes don't block the response) — added latency is invisible to users
- Single global chain has much stronger forensic value than per-replica chains
- Lock contention is bounded — audit writes are infrequent relative to data reads
- The chain becomes a single, verifiable sequence

**Procedure:**
```sql
CREATE FUNCTION platform.audit_insert(
    p_actor TEXT, p_action TEXT, p_entity_id UUID, p_payload JSONB
) RETURNS UUID AS $$
DECLARE
    v_last_hash TEXT;
    v_new_id    UUID := gen_random_uuid();
    v_new_hash  TEXT;
BEGIN
    -- Lock the latest row to serialize chain construction
    SELECT hash INTO v_last_hash
    FROM platform.audit_log
    ORDER BY occurred_at DESC, id DESC
    LIMIT 1
    FOR UPDATE;

    v_new_hash := encode(
        digest(v_new_id::text || now()::text || p_actor || p_action ||
               p_entity_id::text || coalesce(v_last_hash, ''), 'sha256'),
        'hex'
    );

    INSERT INTO platform.audit_log (id, occurred_at, actor, action, entity_id,
                                    payload, prev_hash, hash)
    VALUES (v_new_id, now(), p_actor, p_action, p_entity_id, p_payload,
            v_last_hash, v_new_hash);

    RETURN v_new_id;
END;
$$ LANGUAGE plpgsql;
```

**Consequences:**
- Audit write throughput is bounded by row-lock contention (~5000 writes/sec realistic)
- This is well below platform read throughput, so not a hot-path concern
- Verification job can detect tamper by recomputing the chain end-to-end

---

## ADR-006: Concurrency Primitive for SchemaRegistry

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

The SchemaRegistry is read on every request. With high reconcile churn, a `RwLock` could starve readers when the write lock is held. We need lock-free reads.

**Decision:** Use `arc_swap::ArcSwap<RegistryInner>` instead of `Arc<RwLock<RegistryInner>>`.

**Rationale:**
- Readers never block — `load()` is a single atomic pointer load
- Writers do clone-and-swap (O(N) but writes are infrequent, ~1/second worst case)
- N is bounded by schema count (hundreds, not millions) — clone cost is acceptable
- Standard pattern in performance-critical Rust services

**Implementation:**
```rust
struct SchemaRegistry {
    inner: arc_swap::ArcSwap<RegistryInner>,
}

impl SchemaRegistry {
    fn resolve(&self, path: &SchemaPath) -> Option<Arc<ResolvedSchema>> {
        self.inner.load().by_path.get(path).cloned()
    }

    fn upsert(&self, schema: ResolvedSchema) {
        self.inner.rcu(|current| {
            let mut new = (**current).clone();
            new.by_path.insert(schema.path(), Arc::new(schema));
            new
        });
    }
}
```

**Consequences:**
- Reads are wait-free
- Writes occasionally retry under contention (rare)
- Memory: 2× registry size during swap (acceptable)

---

## ADR-007: Database Connection Role

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

The architecture claims Postgres RLS is a "backstop" enforcing access control even when the application has bugs. But RLS is bypassed when the connecting role has `BYPASSRLS=true` — which is the default for typical app-server connections.

**Decision:** The Velocity API server MUST connect to Postgres as a non-superuser role.

**Specifications:**
- Connection role: `velocity_api` (created by operator at provisioning)
- `velocity_api` has `BYPASSRLS = false` explicitly set
- Per-domain writer/reader roles are granted to `velocity_api`
- Before each transaction, `SET LOCAL ROLE <domain>_writer` selects the appropriate per-domain context
- `SET LOCAL app.current_user`, `SET LOCAL app.current_store_id`, etc. provide ABAC context to RLS policies

**Rationale:**
- Makes RLS an actual backstop, not theatrical
- Per-domain role separation prevents cross-domain data access at the DB level
- A bug in row-filter injection now has DB-level protection

**Consequences:**
- Operator must provision Postgres roles, not just schemas/tables
- Connection setup is more involved (role switch on each transaction)
- Performance impact: negligible (`SET LOCAL` is local to transaction, no IO)

---

## ADR-008: Kafka Topic Strategy

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

Hooks emit events. The original design used per-object-per-event topics: `{org}.{app}.{domain}.{object}.{event}`. With 5 apps × 10 domains × 10 objects × 4 events = 2000 topics in a moderate deployment. Kafka topic management overhead grows linearly with schema count.

**Decision:** Per-domain topic with structured headers.

**Topic naming:**
```
velocity.{org}.{app}.{domain}.events
```

**Message format:**
```json
{
  "schema":    "purchase-order",
  "version":   "v2",
  "event":     "created",
  "entity_id": "uuid",
  "actor":     "ravi.kumar@acme.com",
  "occurred_at":"2026-05-16T10:32:14Z",
  "payload":   { ... }
}
```

**Headers:**
```
velocity-schema:  purchase-order
velocity-event:   created
velocity-version: v2
```

Consumers filter by header. Per-schema topics still available as opt-in via `hook.target.dedicated_topic: true`.

**Rationale:**
- Topic count is bounded by domain count (≈ 50, not 2000)
- Consumer ergonomics: subscribe to one topic per domain of interest, filter in code
- ACLs are per-topic — domain-level isolation is natural
- Topic lifecycle is per-domain (when a domain is deleted, one topic is deleted)

**Consequences:**
- All hook events for a domain flow through one Kafka topic
- Consumers must filter by header (small CPU cost, negligible)
- Per-partition ordering is per-domain (matches the natural transactional boundary)

---

## ADR-009: Pagination Strategy

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

Offset-based pagination breaks for large datasets: records may shift between pages, repeated visits return duplicates or skip records. For 500M-record tables, offset is also slow (Postgres still scans skipped rows).

**Decision:** Hybrid — cursor-based for large result sets, offset still allowed for small queries.

**Rules:**
- Default: cursor-based pagination
- Offset allowed only when `limit + offset ≤ 1000`
- Above that threshold, server returns 400 with message: "Use cursor-based pagination for large result sets"

**Cursor format:**
```
Opaque base64-encoded JSON: {"o": "{order_field}", "v": "{last_value}", "id": "{last_id}"}
```

The cursor is signed with HMAC to prevent tampering.

**API:**
```
GET /api/.../v2?limit=50&cursor=eyJvIjoiY3JlYXRlZF9hdCIsInYiOiIyMDI2..."

Response:
{
  "data": [...],
  "pagination": {
    "next_cursor": "eyJ...",
    "has_more": true
  }
}
```

**Rationale:**
- Cursors guarantee correctness across pages even with concurrent writes
- HMAC prevents clients from constructing arbitrary cursors (and potentially scanning data they shouldn't see)
- Offset retained for small queries where its semantics are acceptable

---

## ADR-010: Multi-Tenancy Model

**Status:** Accepted
**Date:** 2026-05-16
**Context:**

The hierarchy implies multi-tenancy (multiple orgs in one deployment) but the model was not explicit.

**Decision:** Velocity supports two deployment modes:

**Single-tenant** — one Velocity deployment serves one Organisation. Default mode. Used by enterprises running Velocity for their own teams.

**Multi-tenant** — one Velocity deployment serves multiple Organisations. Used for managed/SaaS offerings.

**Isolation guarantees in multi-tenant mode:**

| Layer | Isolation mechanism |
|-------|---------------------|
| Kubernetes | Each org's CRDs in `{org}-*` namespaces; k8s RBAC enforces visibility |
| Postgres | Per-domain schemas already separate orgs naturally; per-org Postgres role hierarchy |
| Cross-org references | Forbidden — operator rejects `ref` to schemas in other orgs |
| Search | Typesense API key scoped per org collection |
| Logs | Multi-tenant Loki with org label, RBAC at query time |
| Metrics | Org label on all metrics |
| Audit | Per-org audit log (logical, via schema_org filter) |

**Rationale:**
- Velocity's hierarchy was designed for this from the start; just needs to be explicit
- Avoids forcing single-tenant deployments to set up unnecessary isolation
- Multi-tenant mode has stricter guarantees applied uniformly

**Consequences:**
- The platform must support both modes at the operator level (configurable)
- Cross-org refs need explicit rejection in the validating webhook
- Documentation must cover both modes
