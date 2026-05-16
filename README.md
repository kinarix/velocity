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
  velocity-operator/      kube-rs reconcilers (Org/App/Domain in Phase 0)
  velocity-api/           Axum API server (Phase 2+)
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

Prereqs: Docker, Rust 1.83+, kubectl, helm, minikube, openssl.

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

### 4. End-to-end on minikube (Phase 0 acceptance)

```bash
minikube start
make e2e
```

`make e2e` builds the webhook image into minikube's daemon, helm-installs
the chart, runs the operator on the host against docker-compose Postgres,
applies a `Domain`, and verifies the four Phase 0 acceptance checks:

1. Webhook denies a namespace mismatch.
2. Webhook admits a well-formed Domain.
3. Operator provisions the per-domain Postgres schema.
4. `Domain.status.phase` reaches `Ready`.

The script tears down the resources it creates on exit.

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
