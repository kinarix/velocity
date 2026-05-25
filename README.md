# Velocity

Schema-driven, Kubernetes-native backend platform in Rust.

Apply a `SchemaDefinition` CRD; the platform provisions a Postgres table, REST
API, validation, search, auth, audit, time machine, and observability —
automatically. See [`docs/architecture.md`](docs/architecture.md) (TBD) and
[`docs/design.md`](docs/design.md) for the long-form story.

> **Status: Phase 0** — Hierarchy (Org → App → Domain), validating webhook,
> Postgres provisioning, audit chain, CRD generation. Everything else is on
> the [phases roadmap](docs/phases.md).

---

## Layout

```
crates/
  velocity-types/         CRD structs + ResolvedSchema + fail-mode matrix
  velocity-core/          Shared API library: auth, SchemaRegistry, config,
                          audit-write, schema/access model, bootstrap
  velocity-operator/      kube-rs reconcilers (Org/App/Domain in Phase 0)
  velocity-data-api/      Data plane: CRUD/query/time-machine/archive (links core)
  velocity-platform-api/  Admin/UI + CRD read-write + platform audit + SPA (links core)
  velocity-search/        Tier-3 Typesense search + CDC worker (links core)
  velocity-warm-reader/   Warm-tier Parquet/DataFusion read service (Phase 4+)
  velocity-typesense/     Shared Typesense client + collection specs
  velocity-webhook/       ValidatingWebhook server
  velocity-cli/           `velocity` binary (Phase 1+)
  velocity-archive-worker/ batch archive worker (Phase 4+)
  velocity-log-collector/ DaemonSet log shipper (Phase 5+)
  velocity-log-processor/ enrichment + filter + route (Phase 5+)
charts/velocity/          Helm chart (operator + webhook + CRDs)
crds/                     Generated CRD YAML (do NOT edit — see `generate-crds`)
migrations/               platform.* SQL (numeric order)
db/init/                  Bootstrap roles applied at first container start
tests/e2e/                Phase-level acceptance scripts
docs/                     Architecture, design, decisions, operations, phases
runbooks/                 Operational runbooks (per `docs/operations.md`)
```

---

## Quick start (local dev)

Prereqs: Docker, Rust 1.83+, kubectl, helm, k3d, openssl.

### 1. Start the data plane

```bash
make up               # Postgres (5434), Redis, Kafka, Typesense via docker compose
make db-bootstrap     # Apply db/init/*.sql (idempotent)
make migrate          # Apply migrations/*.sql in order
make db-verify-rls    # Sanity: velocity_api is NOBYPASSRLS (ADR-007)
```

Connection URLs:

```bash
make db-url
```

### 2. Generate CRDs (only after touching CRD structs)

```bash
cargo run --bin generate-crds
# writes crds/*.yaml — regenerate Helm copy:
cp crds/*.yaml charts/velocity/crds/
```

Never hand-edit `crds/*.yaml`.

### 3. Run the operator locally

```bash
make operator         # cargo run -p velocity-operator against docker-compose Postgres
```

The startup gate verifies `velocity_api` is `NOBYPASSRLS` and that
`platform.audit_insert` is installed before doing anything else.

### 4. Full stack in k3d (portal + Phase 0 acceptance)

```bash
make k3d-up           # picker → pick existing cluster or create new; builds + helm installs
echo '127.0.0.1 velocity.local' | sudo tee -a /etc/hosts   # one-time
open http://velocity.local:8080/
make e2e              # Phase 0 acceptance suite against the up cluster
```

`make k3d-up` builds every velocity image via `docker buildx bake`, imports
them into the k3d cluster, applies the CRDs, generates webhook TLS, and
helm-installs the chart with the dev overlay. Portal nginx + ingress is on
by default, so the SPA is reachable at `http://velocity.local:8080/` via
the k3d traefik load balancer.

`make e2e` runs the Phase 0 acceptance suite against the running release:

1. Webhook denies a namespace mismatch.
2. Webhook admits a well-formed Domain.
3. Operator provisions the per-domain Postgres schema.
4. `Domain.status.phase` reaches `Ready`.

Operational helpers:

```bash
make k3d-logs                 # tail aggregate velocity logs
make k3d-logs COMPONENT=api   # scope to one component
make k3d-status               # kubectl get all,ingress in velocity-system
make k3d-redeploy             # rebuild images, import, helm upgrade, roll pods
make k3d-clean                # helm uninstall + delete namespace (keep cluster)
k3d cluster delete velocity   # tear the cluster down entirely
```

---

## Workspace commands

```bash
make build            # cargo build --workspace
make test             # cargo test --workspace
make fmt              # cargo fmt
make clippy           # cargo clippy -D warnings
make audit            # cargo audit
make operator-test    # operator integration tests against docker-compose pg
make db-smoke         # smoke-test the audit chain + ADR-007 gate live
```

---

## Required reading before contributing

1. [`docs/design.md`](docs/design.md) — CRD specs, API contracts, DB conventions
2. [`docs/decisions.md`](docs/decisions.md) — ADRs 001–010 (do not deviate without an ADR update)
3. [`docs/operations.md`](docs/operations.md) — backup, restore, failover
4. [`docs/phases.md`](docs/phases.md) — phased delivery plan
5. [`CLAUDE.md`](CLAUDE.md) — implementation guide (load-bearing)

### Non-negotiables

- **Non-superuser `velocity_api` DB role** (ADR-007). RLS is a real backstop.
- **No raw SQL with user input** — parameterised queries only.
- **`arc_swap::ArcSwap` for the schema registry** (ADR-006). No `RwLock`.
- **Outbox pattern for Tier-3 search** (ADR-002). No direct CDC.
- **CEL bounded to ≤ 10ms** via `tokio::time::timeout`.
- **Audit writes only via `platform.audit_insert()`** stored proc.
- **CRDs are config**, not data. Entity data lives in Postgres.

---

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
