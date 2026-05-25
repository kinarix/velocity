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

---

## ADR-011: Dedicated API Deployment per Velocity Object

**Status:** Proposed
**Date:** 2026-05-24
**Supersedes (on acceptance):** revises ADR-001 and ADR-006; touches ADR-007.

> This ADR is **Proposed**, not Accepted. It records the analysis of a
> requested re-architecture and the decisions that must be made before
> any code lands. ADR-001 (informer per replica), ADR-006 (arc_swap
> registry), and ADR-007 (DB connection role) remain authoritative until
> this ADR is Accepted. Phase 12 in [`phases.md`](phases.md) is the
> delivery vehicle.

### Context

Today Velocity runs **one** `velocity-api` Deployment. Each replica runs
a kube informer that builds an in-memory `ArcSwap<RegistryInner>`
holding *every* `ResolvedSchema` in the cluster (ADR-001 + ADR-006). The
Axum router is static; handlers extract the `org/app/domain/object/version`
path from the URL and resolve it against the registry on every request.
One fleet, scaled by HPA + KEDA on aggregate load, serves all schemas.

The proposed change: treat a `SchemaDefinition` **plus its related CRDs**
(the `AuthStrategy` it references, its `RoleBinding`s, `ArchivePolicy`,
`LogFilterPolicy`, …) as a single **Velocity object**, and have the
operator **provision a dedicated API Deployment per Velocity object on
the fly**. One API deployment serves exactly one schema.

This inverts the platform's central data-plane decision. It is a move
from a *shared, dynamically-routed data plane* to a *control-plane that
materialises one workload per schema* — the operator stops being only a
Postgres/Typesense provisioner and becomes a **workload orchestrator**.

### What the proposed model buys

- **Blast-radius isolation.** A poison schema, a runaway CEL rule, a
  memory leak, or a panic affects only that schema's pod — not all
  traffic. Noisy-neighbour isolation is structural, not best-effort.
- **Independent scaling.** A hot schema scales on its own HPA/KEDA curve;
  today a single hot schema forces the whole fleet to scale.
- **Independent rollout / versioning.** Different schemas can run
  different API binary versions — canary a platform upgrade on one
  schema before the rest.
- **Tighter least-privilege.** A per-schema pod can be handed only its
  own per-domain DB role credentials and its own Typesense scoped key. A
  compromised pod sees one schema's data, not the catalogue.
- **Simpler data-plane code.** The dynamic registry, the informer-fed
  `ArcSwap`, and path-parameter schema resolution largely disappear from
  the data API — routing is fixed to one schema, resolved once at boot.
- **Natural per-schema accounting** (chargeback, quota, metrics).

### What it costs — and the four things the request under-specifies

**1. "One per schema" is ambiguous. This is the headline decision.**
"Postgres schema" in this codebase already means *domain*
(`pg_schema_name(org, app, domain)`), and the namespace is
`{org}-{app}-{domain}`. So "one deployment per schema" has three
readings with very different blast radius and cost:

| Granularity | One deployment per… | Count (moderate deploy) | Isolation | In-process cross-schema |
|-------------|---------------------|--------------------------|-----------|--------------------------|
| **per-SchemaDefinition** (literal) | object × version | hundreds | finest | none |
| **per-Domain** (recommended) | `{org}-{app}-{domain}` | tens | strong | works *within* a domain |
| per-App / per-Org | app or org | few | coarse | works across app/org |

Per-Domain aligns with **every isolation boundary that already exists**:
the Postgres schema, the per-domain roles (ADR-007), the Kafka topic
(ADR-008), the namespace, and the multi-tenancy model (ADR-010, which
already forbids cross-*org* refs). A per-domain pod serves all its
`SchemaDefinition`s in one process, so cross-schema joins *inside a
domain* — the common case — need no RPC. The request says "schema";
this ADR's recommendation is **per-Domain**, with per-SchemaDefinition
available as an opt-in (`spec.deployment.scope: domain`) for domains that need
maximal isolation. **Decision required.**

**2. This does not eliminate the shared tier — it splits the data plane in two.**
Several features cannot live in a single-schema pod, because no such pod
sees more than one schema:

- Cross-schema search — `POST /api/{org}/search` over the unified
  per-org Typesense collection.
- Cross-schema query `include[]` joins and the Layer-3 cross-schema RBAC
  gate (`CROSS_SCHEMA_ACCESS_DENIED`).
- Platform endpoints — `GET /api/platform/audit`, `/audit/verify`,
  drift, aggregate health.
- The admin-read endpoints the new UI needs (see §UI).

The honest target architecture is therefore **two tiers**:

- **platform-API** — a *shared* deployment that keeps the ADR-001
  informer + ADR-006 `ArcSwap` registry of all schemas. Owns
  cross-schema search, cross-schema query, platform/audit endpoints,
  admin-read/CRD-listing endpoints, and the UI's backend.
- **data-API** — the per-{schema|domain} deployments the operator
  materialises. Each owns CRUD + single-schema query/search + time
  machine + archive for its own schema(s).

The request as written omits this split; naming it is part of the
analysis. ADR-001/006 are **not retired** — they move to the
platform-API.

**3. Postgres connection fan-out is the cost ceiling.**
Today: ~1 Deployment × N replicas × pool_size ≈ tens of connections.
Per-SchemaDefinition with 100 schemas × min 2 replicas (HA) × pool 10 =
**~2 000 connections** against a CNPG `max_connections` of ~100–200.
This makes **PgBouncer (transaction pooling) a hard prerequisite, not an
option**, and even then idle pools waste server-side memory. Per-Domain
is roughly 50× cheaper on connection count and is the main quantitative
argument for that granularity. Idle-pod cost compounds it: per-object,
hundreds of pods sit at ≥1 replica even at zero traffic unless we adopt
**scale-to-zero** (KEDA/Knative), which trades idle cost for cold-start
latency on first request.

**4. The operator becomes a workload controller.**
It must now own — with owner references and garbage collection — a
Deployment + Service + HPA + ConfigMap + Secret + ingress route per
Velocity object; do rolling updates on spec change; sequence DDL
migration against pod rollout (migrate-then-rollout for additive,
gated for breaking); and damp reconcile storms (a parent Org/App policy
change cascades to many simultaneous rollouts — the existing jittered
cascade in CLAUDE.md must extend to pod rollouts). Operator RBAC expands
from "Postgres + CRD status" to "create/patch/delete Deployments,
Services, Ingresses, HPAs." Ingress route count grows with schema count;
host- or path-based routing (or Gateway API `HTTPRoute` per object) must
be generated and kept collision-free.

### Decisions required before acceptance

1. **Granularity** — per-Domain (recommended) vs per-SchemaDefinition
   (literal) vs opt-in hybrid via `spec.deployment.scope`.
2. **Cross-schema / platform tier** — confirm the platform-API +
   data-API split; platform-API retains ADR-001/006.
3. **Idle cost** — min-replicas (always-on, simpler) vs scale-to-zero
   (KEDA/Knative, cold starts).
4. **Connection pooling** — adopt PgBouncer transaction pooling as a
   platform prerequisite; set per-pod pool ceilings.
5. **Routing** — Ingress-per-object vs shared Ingress with generated
   paths vs Gateway API `HTTPRoute`.

### Interaction with the other requested changes

- **Anonymous auth mode** (Phase 12b) is *not* "remove auth." ADR-005's
  `platform.audit_insert(p_actor TEXT, …)` and ADR-007's
  `SET LOCAL app.current_user` both require an identity. Anonymous mode
  must inject a fixed `Identity { actor_id: "anonymous", roles: [],
  strategy: "none" }` and skip verification — a *bypass*, not a
  *removal*. Audit chain and RLS context stay intact. Gated by an
  operator/platform flag, off by default, loud in logs and `/readyz`.
- **UI** (Phase 12c) needs admin-read endpoints to list CRDs — the
  Phase 10 "Deferred" list already records that *no admin read API
  exists*. The tree panel (Org → App → Objects + org-level objects) and
  the CRD-schema-aware "Edit as YAML" editor depend on (a) new
  admin-read endpoints on platform-API, and (b) the CRD OpenAPI schema
  from kube-apiserver (`/openapi/v3/apis/velocity.sh/v1`). Both are real
  work, not free.
- **Portal in flight.** The portal→API static-serving change currently
  in the working tree predates this UI vision. The new requirements
  **supersede** the Phase 10 portal scope; the new UI is served by
  platform-API and the standalone nginx portal is retired.
- **CLI** is demoted to headless/gitops use — feature parity is no
  longer required for interactive flows, but `apply`/`get`/`diff` remain
  the gitops substrate.

### Recommendation

Accept the **two-tier split** (platform-API shared + data-API
materialised) at **per-Domain** granularity, with per-SchemaDefinition
as opt-in, **PgBouncer mandatory**, **min-replicas** first (defer
scale-to-zero), and **shared Ingress with operator-generated paths**.
This delivers the isolation the request wants while keeping connection
fan-out and operator complexity bounded, and it preserves ADR-001/006 in
the one place they still make sense.

### Consequences (if accepted)

- ADR-001 and ADR-006 are revised to scope to platform-API; the data-API
  gets its single schema injected at boot (no per-pod informer needed
  for per-SchemaDefinition mode; a narrow per-domain informer for
  per-Domain mode).
- ADR-007 is unchanged in substance (roles, `SET LOCAL ROLE`,
  `NOBYPASSRLS`) but pool sizing moves behind PgBouncer.
- `velocity-types` gains a `deployment` block on `SchemaDefinition`
  (and/or `Domain`); the operator gains a workload-orchestration
  controller; `velocity-api` gains a `--single-schema`/platform mode
  switch.
- `design.md` and `architecture.md` are rewritten only **after** this
  ADR is Accepted and the five decisions are settled.

### Decisions settled during implementation (2026-05-24)

The five open decisions were resolved as: **per-Domain** granularity
(per-SchemaDefinition deferred as opt-in); **PgBouncer** adopted; **min
replicas** via `minReplicaCount ≥ 1` (no scale-to-zero); **shared host with
operator-generated per-domain Ingress paths**; platform/data split confirmed.
Three further decisions surfaced while building Phase 12a:

- **UI writes through the platform-API** (not gitops-only): platform-API
  gains authenticated CRD create/update/delete endpoints that apply to kube
  with the validating webhook still in the path.
- **Separate binary crates for the two tiers** (revises the original
  "same binary, mode switch" note). The shared data-plane code stays in the
  `velocity-api` **library** crate; two thin binary crates depend on it:
  `velocity-data-api` (per-domain data plane) and `velocity-platform-api`
  (the library + admin/UI/cross-domain/CRD-write surface). The admin write
  handlers (`platform_objects`, CRD-write, OpenAPI proxy) live in the
  platform-api crate, so the **data-API binary never contains admin-write
  code** — making the least-privilege claim structural, not just a router
  that declines to mount routes. `VELOCITY_API_MODE` is retired in favour of
  the binary you run.

  **Responsibility boundary (cross-domain, not cross-schema):** a data-API
  serves everything scoped to its single domain — CRUD, query *including
  joins between schemas in the same domain*, time machine, archive — because
  its namespaced informer holds all the domain's SchemaDefinitions in-process.
  The platform-API owns only what spans domains/orgs: cross-domain
  `include[]`, platform audit, admin reads/writes, and the UI.

### Final service topology (2026-05-24)

The data/control plane decomposes into these crates/services (the operator
and validating webhook are the control plane; everything below is built on
the shared `velocity-api` **library** crate of common handlers/registry/auth):

| Service (binary crate) | Owns | Scope |
|---|---|---|
| `velocity-platform-api` | admin/UI backend: CRD read **and write** (webhook in path), OpenAPI proxy, hierarchy reads | shared, always-on |
| `velocity-data-api` | data plane only: CRUD, query DSL (Tier-1 filters + Tier-2 Postgres FTS), time machine, archive — **no Typesense, no admin** | per-domain, on-demand (operator-materialised) |
| `velocity-search` | **all** search: per-schema, per-domain, cross-domain, cross-org; owns the `/search` + `/api/{org}/search` endpoints, the CDC outbox→Typesense workers, and Typesense collection/alias management | shared, always-on |
| `velocity-warm-reader` (exists) | warm-tier time-machine reads (DataFusion over S3 Parquet) | shared |

Consequences of pulling search out:
- `velocity-search` runs a full informer-fed registry (it must resolve any
  schema the caller may search) + the auth middleware (to identify the caller
  and apply per-schema read RBAC before searching) + the Typesense client.
- The CDC outbox→Typesense workers move out of `velocity-api` into
  `velocity-search`; the data-API write path still inserts the outbox row in
  the same transaction (ADR-002 unchanged) — `velocity-search` drains it.
- The data-API binary contains no Typesense client and no admin code: its
  attack surface and dependency footprint shrink to Postgres + kube informer.
- Tier-3→Tier-2 search fallback (ADR-003) lives in `velocity-search`, which
  therefore also holds a Postgres connection for the FTS fallback path.
- `VELOCITY_API_MODE` is retired; the running binary determines the role.

**Data-API scope is operator-managed, never global.**
The platform-API does **no** data CRUD. By default (`spec.deployment.scope:
app`, the effective default when unset on both Application and Domain), the
operator materialises one **app-scoped `velocity-data-api`** pod per
`{org}/{app}` in a new `{org}-{app}-shared` namespace, watching all of that
app's domain namespaces via a label selector. A domain with `deployment.scope:
domain` additionally gets its own namespace-scoped `velocity-data-api` in
`{org}-{app}-{domain}` whose operator-generated Ingress path
(`/api/{org}/{app}/{domain}`) overrides the app pod for that domain. Same
binary either way — only the `VELOCITY_API_NAMESPACE` vs
`VELOCITY_API_LABEL_SELECTOR` scope differs. No global shared pod; all data-API
pods are operator-materialised on demand.
- **The operator projects the data-API env Secret** into each dedicated
  domain's (dynamically-created) namespace. It reads a source Secret in its
  own/system namespace (`VELOCITY_OPERATOR_DATA_API_ENV_SOURCE_SECRET`) and
  applies an owner-ref'd copy named `VELOCITY_OPERATOR_DATA_API_ENV_SECRET`
  into the domain namespace, because Helm secrets land in the release
  namespace but domain namespaces are created on demand.
- **The operator mints per-domain Postgres credentials.** Rather than every
  data-API sharing one `velocity_api` password, the operator provisions a
  per-domain LOGIN role `{schema}_api` (`NOSUPERUSER`, `NOBYPASSRLS`, member
  of the domain's reader/writer/admin roles) and carries its operator-owned
  password in the projected env Secret. This realises the ADR's
  least-privilege benefit at the DB layer: a compromised data-API pod can
  authenticate only as its own domain's role. The data-API connects as
  `{schema}_api` and `SET LOCAL ROLE`s into the domain roles per request
  (ADR-007 unchanged). Password is owned by the operator and re-asserted
  idempotently each reconcile (read-existing-or-generate, never rotated
  underneath a running pod without intent).

**Boot model (answering a design question raised during 12a):** at a fresh
boot with no Domains, **zero data-API pods exist**. Always-on at boot:
the operator, the validating webhook, and **one shared platform-API** (which
serves the UI, admin read/write endpoints, cross-schema search, and platform
audit — and is how the first Org/App/Domain is created). Per-domain data-API
pods are materialised on demand when an Application is applied (app-scope) or
when a Domain with `deployment.scope: domain` is applied, and GC'd when the
owning resource is deleted.

**Revision (2026-05-25) — crate cleanup: shared lib renamed + tier code
physically relocated.** This ADR's "shared `velocity-api` library crate" was
renamed **`velocity-core`**, and the tier-specific code was moved out of it
into the binary crates (each now lib+bin), so isolation is structural rather
than a feature flag:

- `velocity-search` now physically owns `cdc` + `typesense` + the search
  handlers; the earlier `search` Cargo feature on the shared lib is **removed**.
- `velocity-data-api` owns the data plane (`handlers`/`dsl`/`tiering`/
  `time_machine`/`archive_handlers`/`event_log`/`idempotency`/`session`).
- `velocity-platform-api` owns `platform_handlers` + `audit_query` +
  `static_files` (the embedded SPA) + the platform router.
- `velocity-core` is the pure shared foundation (auth, `SchemaRegistry`,
  config, audit-write, the schema/access model, `handler_util`, `cursor`,
  `server` bootstrap, `build_auth`). Each tier defines its own state struct
  (`DataState` / `PlatformState` / `SearchState`).

`cargo tree -e normal` confirms no tier links another tier. The Postgres
role `velocity_api` and the `VELOCITY_API_*` env prefix are intentionally
unchanged — only the Rust crate path `velocity_api::` became `velocity_core::`.
