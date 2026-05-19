# Architecture

> What Velocity is, the moving parts that make it work, how data flows
> through them, and where each piece currently lives in the codebase.
> For long-form CRD specs, API contracts, and DB conventions, see
> [`design.md`](design.md). For the ADRs that lock these choices in,
> see [`decisions.md`](decisions.md). For the phased delivery plan and
> what's actually shipped vs deferred, see [`phases.md`](phases.md).

---

## The 30-second pitch

Velocity is a schema-driven, Kubernetes-native backend platform built
in Rust. A developer applies a `SchemaDefinition` CRD; the platform
provisions a Postgres table, a REST API, validation, role-based
access, search, audit, time-machine history, and observability — all
without per-schema code. The platform itself is org-agnostic: `org`
is the first segment of the `org/app/domain/object/version` path,
configured at deploy time.

The whole platform sits on three load-bearing decisions:

1. The schema **is** the wire contract, the DDL, the validation, the
   RBAC surface, the search index spec, the audit policy. There is no
   second source of truth.
2. The data plane (API) is fed by a Kubernetes **informer per
   replica** ([ADR-001](decisions.md)) — never by RPC from the
   operator — and reads its registry through an
   `arc_swap::ArcSwap` ([ADR-006](decisions.md)) so the hot path is
   lock-free.
3. The DB role is **non-superuser, `NOBYPASSRLS`**
   ([ADR-007](decisions.md)) from the very first Postgres
   connection, so row-level security is a real backstop instead of
   theatre.

---

## Top-down view

```
┌─────────────────────────── Developer plane ─────────────────────────────┐
│   velocity CLI ──┐                                                       │
│   kubectl apply ─┴──→ ValidatingWebhook ──→ kube-apiserver (CRDs)        │
│   Admin Portal (Phase 10)                                                │
└──────────────────────────────────────────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────── Control plane ───────────────────────────────┐
│   velocity-operator (kube-rs, leader-elected)                            │
│     • HierarchyController     (Org / Application / Domain)               │
│     • SchemaDefinitionController                                         │
│     • ArchivePolicyController + PurgeRequestController                   │
│     • RoleBindingController                                              │
│     • LogFilterPolicy / LogRoutingPolicy sweepers                        │
│     • SLO → PrometheusRule sweeper                                       │
│     • Drift sweep (hourly)                                               │
│   velocity-archive-worker  (independent deployment; batched data motion) │
└──────────────────────────────────────────────────────────────────────────┘
                                  │
                       provisions │ owns
                                  ▼
┌─────────────────────────── Data plane ──────────────────────────────────┐
│   velocity-api  (Axum; informer-fed SchemaRegistry; /readyz gated)      │
│       ↑                                                                  │
│       │  generic CRUD + DSL query + search + audit + time-machine        │
│       │  (single set of handlers, no per-schema code)                    │
│       ▼                                                                  │
│   ┌────────────┐  ┌──────────┐  ┌───────────┐  ┌────────────┐           │
│   │ Postgres   │  │  Redis   │  │ Typesense │  │   Kafka    │           │
│   │ CNPG HA    │  │ revoke   │  │  Tier 3   │  │   hooks    │           │
│   │ + *_archive│  │ list +   │  │  search   │  │   alerts   │           │
│   │ + history  │  │ idemp.   │  │           │  │            │           │
│   └─────┬──────┘  └──────────┘  └────┬──────┘  └────────────┘           │
│         │ outbox (Tier 3)            │                                   │
│         └────── CDC worker ──────────┘                                   │
│                                                                          │
│   velocity-warm-reader  (DataFusion over S3 Parquet, Phase 4)            │
│   velocity-log-processor  (enrich → filter → route)                      │
│   velocity-log-collector  (DaemonSet, tails pod stdout)                  │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## Component table

| Component | Crate | Runtime | What it owns | Phase |
|-----------|-------|---------|--------------|-------|
| CRD types | `velocity-types` | library | All CRD structs, `generate-crds` binary, common types (`Condition`, `ReconcilePhase`, `Identity`, fail-mode matrix) | 0 |
| Operator | `velocity-operator` | Deployment (HA, leader-elected) | Reconcilers for Org/App/Domain, SchemaDefinition, ArchivePolicy, PurgeRequest, RoleBinding, LogFilterPolicy, LogRoutingPolicy. Postgres provisioning (hot + archive schemas, roles, RLS). Typesense provisioning + blue-green collection swap. SLO → PrometheusRule sweep. Hourly drift sweep. | 0–8 |
| Validating webhook | `velocity-webhook` | Deployment (3 replicas) | Last line of defence on CRD applies: namespace match, CEL safety, quota, cross-domain refs, per-CRD invariants | 1 |
| API | `velocity-api` | Deployment (HPA + KEDA) | Generic CRUD, DSL query, FTS, Typesense search, audit read/verify, time-machine endpoints, archive endpoints, idempotency, RBAC layers 1–7, CDC worker (per Tier-3 schema), Prometheus metrics middleware | 1–8 |
| Archive worker | `velocity-archive-worker` | Deployment | Single-tx `archive_batch` / `purge_batch` primitives, S3 Parquet destination, tick loop driven by ArchivePolicy spec | 8 |
| Warm-tier reader | `velocity-warm-reader` | Deployment | DataFusion over S3 Parquet for time-machine reads outside the 90-day hot window; HTTP RPC behind a bearer token | 4 |
| Log processor | `velocity-log-processor` | Deployment | Enrichment (velocity.{org,app,domain,schema} labels), filter rules (keep / drop / sample / redact), routing to Loki / S3 / Kafka | 6b |
| Log collector | `velocity-log-collector` | DaemonSet | Tails pod stdout JSON, ships to log-processor | 6b |
| Typesense client | `velocity-typesense` | library | Shared client used by operator (provisioning, alias flip) and api (CDC + search) | 5 |
| CLI | `velocity-cli` | binary | `apply`, `audit verify`, `drift check`, `reconcile`, `status` and friends | 4.5–9 |

Every crate forbids `unsafe_code` at the workspace level and denies
`unused_must_use`. `unwrap_used`, `expect_used`, `panic`, and
`print_{stdout,stderr}` are warn-level — the few legitimate uses
carry an explicit `#[allow]` with a justification.

---

## Two persistence boundaries you must understand

**1. The CRD store (etcd) is the control-plane source of truth.**
CRDs are config. Velocity never stores entity data in CRDs. The
operator and the API server both watch the CRD store through kube
informers — they don't talk to each other directly. Restarting the
operator does not interrupt the API server; restarting the API
server does not interrupt reconciliation.

**2. Postgres is the data-plane source of truth.** Everything else
— Typesense indexes, Parquet exports, Loki logs — is derived.
Recovery after a disaster is `kubectl apply -f gitops/` (CRDs) +
CNPG PITR (Postgres) + reindex (Typesense). The runbook for this
lives in [`operations.md`](operations.md) §2.

The bridge between them is **the outbox table**
([ADR-002](decisions.md), [`design.md`](design.md) §3.4). For Tier-3
search schemas, the write transaction inserts into the main table
and the outbox table atomically; a CDC worker tails the outbox with
`FOR UPDATE SKIP LOCKED` and ships to Typesense. If Typesense is
down, writes still succeed and the backlog drains when it returns.

---

## Request lifecycle — write path

```
client → axum
  ↓ middleware
  auth (JWT / OIDC / API key per AuthStrategy)
  ↓
  revocation check (Redis, fail-closed by default per ADR-003)
  ↓
  identity built (actor_id, roles, attributes, strategy, issuer)
  ↓ handler
  registry.resolve(path) → ResolvedSchema   (lock-free, ArcSwap)
  ↓
  RBAC layers 1–7  (route → ABAC → cross-schema → row filter →
                    field filter → masking → RLS)
  ↓
  validation  (type / required / enum / range / pattern / compiled CEL
               with bounded execution per ADR S2)
  ↓
  idempotency check (platform.idempotency_keys, replay if hash matches)
  ↓
  BEGIN
    SET LOCAL ROLE <domain_writer>       (ADR-007)
    SET LOCAL app.current_user = $actor  (RLS + audit context)
    INSERT INTO <hot> (...)              (single trigger fires:
    -- trigger writes:                      history + outbox in one tx)
    --   INSERT INTO <hot>_history (...)
    --   INSERT INTO <hot>_outbox  (...)  -- Tier-3 only
    platform.audit_insert(...)           (stored proc; chains hash)
  COMMIT
  ↓
  response + X-Request-ID + (cached for idempotency replay)
```

If any step fails after auth, the rejection is itself audited — denial
auditing landed in Phase 6a. The outbox row is published to Typesense
by the CDC worker on a separate connection; the API never blocks on
Typesense.

---

## Request lifecycle — time-machine read

```
GET /{id}/history?at=T

  age(T)  <  90 days  →  Postgres hot tier (<table>_history)
          <   5 years →  velocity-warm-reader  (DataFusion over S3 Parquet)
          ≥   5 years →  202 Accepted + job_id  (cold tier; deferred fulfilment)

GET /{id}/archive       →  *_archive.<table>  (Phase 8 slice 9)
POST /archive/query     →  *_archive.<table>  (paginated)
POST /{id}/unarchive    →  clears hot row's archived_at + drops archive copy
                           (single tx; 410 ARCHIVE_HOT_ROW_PURGED if hot row
                            has been purged via ArchivePolicy.purgeAfter)
```

The warm tier is reached by HTTP RPC over a bearer token with
constant-time comparison; see "Inter-Service RPC" in
[`CLAUDE.md`](../CLAUDE.md). It is not a service mesh; it is a single
internal hop with explicit failure semantics (ADR-003 alignment).

---

## What is not in the binary today

The phased plan in [`phases.md`](phases.md) is the authoritative
checklist; this section calls out the larger absences so a casual
reader doesn't assume they're behind a feature flag.

- **Version lifecycle** — schema deprecation is informational only;
  the `410 Gone` response path and the `VersionOperator` state
  transitions described in the Phase 8 plan are deferred to a future
  slice.
- **Cold-tier fulfilment** — the API returns `202 + job_id` for cold
  queries but the Glacier retrieval worker is not yet implemented.
  The interface exists; the back-end does not.
- **CEL trigger for ArchivePolicy** — the spec is validated, but the
  per-row evaluator inside the archive worker is not wired. CEL
  triggers are silently skipped.
- **S3 orphan parquet recovery sweep** — a crash between Parquet
  upload and hot-row marking can leave an orphan; cleanup is manual
  for now (see [`operations.md`](operations.md) §2a).
- **Sharded archive workers with `FOR UPDATE SKIP LOCKED`** — the
  archive primitive is single-writer; horizontal scale is a follow-up.
- **Admin Portal** — Phase 10.
- **CLI feature coverage** — what shipped is `apply`, `audit verify`,
  `drift check`, `reconcile`, `status`. The full Phase 9 command set
  is a future phase.
- **Hardening pass** — load tests, chaos drills, dependency audit
  cadence (Phase 11).

---

## Invariants that are not negotiable

These are enforced in code, in CI, or in the validating webhook.
Don't write a path that violates one.

- DB connection role is `velocity_api`, `NOBYPASSRLS`. API server
  refuses to start if the role bypasses RLS.
- All transactions `SET LOCAL ROLE <domain_role>` and
  `SET LOCAL app.current_user` before any data access.
- Table and schema names in SQL come from `SchemaRegistry`, never
  from path parameters. All values are bound parameters.
- CEL is compiled at schema load, never on the hot path, and every
  evaluation runs under a `tokio::time::timeout` of at most 10 ms.
- Audit log inserts go through `platform.audit_insert(...)`; direct
  inserts are revoked at the GRANT level.
- API key plaintext is shown once and never stored; only the SHA256
  is persisted.
- Metric label cardinality is bounded — entity IDs and user-supplied
  strings never become label values; they go into traces and logs.
- The validating webhook is `failurePolicy: Fail` for everything
  except the three bootstrap CRDs (Org / Application / Domain),
  which are `Ignore` so the cluster has an escape hatch.
- For Tier-3 search schemas, the data write and the outbox write are
  in the same transaction. Either both commit or neither does.
- For PurgeRequest, the `velocity.sh/approved-by` annotation is the
  only path to approval. There is no programmatic auto-approval.

---

## Where to read next

- [`design.md`](design.md) — CRD specs (full YAML), API contracts,
  DB conventions, error shapes
- [`decisions.md`](decisions.md) — ADRs 001–010 (why each load-bearing
  choice is what it is)
- [`phases.md`](phases.md) — phased plan with shipped-vs-deferred
  for each delivered phase
- [`operations.md`](operations.md) — backup, restore, failover,
  archive / purge operations, runbooks
- [`CLAUDE.md`](../CLAUDE.md) — implementation guide for anyone
  (human or model) writing Velocity code
