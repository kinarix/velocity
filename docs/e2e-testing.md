# End-to-End Testing — Local k8s (k3d)

This guide walks through running Velocity end-to-end on a local **k3d**
cluster. The flow is split across two Make targets:

| Target | What it does |
|---|---|
| `make k3d-up` | Picks (or creates) a k3d cluster, builds every velocity image with `docker buildx bake`, imports them into the cluster, applies CRDs, generates webhook TLS, and helm-installs the chart with the dev overlay. Portal nginx + ingress are on by default. |
| `make e2e` | Runs the Phase 0 acceptance suite against the already-up release. |

Orchestration lives in `scripts/k3d-up.sh`; the acceptance script lives at
`tests/e2e/run.sh`. Infrastructure (Postgres + Redis + Kafka + Typesense +
Minio) runs on the host via `docker compose`; each velocity service inside
the cluster reaches it at `host.k3d.internal`, wired through the chart's
top-level `globalHost` value.

Image builds use **`docker buildx bake`** (targets declared in
`docker-bake.hcl`), which builds all six images in parallel under a single
BuildKit instance. Set `VELOCITY_PROGRESS=plain` to fall back to a
streaming log if you need the full output.

---

## Two modes

| Mode | Trigger | What it brings up in-cluster |
|---|---|---|
| **Full stack (default)** | `make k3d-up` | operator + webhook + api + portal + warm-reader + archive-worker. Build cost: 5 Rust images + portal, ~3-5 min cold. |
| **Minimal** | `VELOCITY_MINIMAL=1 make k3d-up` | webhook only. Operator must be run out-of-cluster yourself via `make operator`. Build cost: 1 image, ~90 s cold. For fast iteration on webhook code. |

The acceptance suite runs the same four Phase 0 checks in both modes (see
`docs/phases.md`):

1. CRDs install cleanly.
2. Webhook **rejects** a `Domain` whose namespace mismatches `{org}-{app}`.
3. Webhook **admits** a well-formed `Domain`.
4. Operator provisions the Postgres schema + roles; `Domain.status.phase`
   reaches `Ready`.

Full-stack mode additionally deploys the API, portal, warm-reader, and
archive-worker so you can exercise CRUD, the UI, and the archive pipeline.

---

## 1. One-time prerequisites

Required on `PATH`:

- `docker` (with `docker compose`)
- `kubectl`
- `helm`
- `k3d`
- `openssl`
- `cargo` (Rust — see `rust-toolchain.toml`)

Quick install on macOS:

```bash
brew install docker kubectl helm openssl k3d
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Add a host entry so the browser can resolve the portal ingress:

```bash
echo '127.0.0.1 velocity.local' | sudo tee -a /etc/hosts
```

`make k3d-up` warns and prints this command if `/etc/hosts` is missing it.

---

## 2. Bring up the shared host infrastructure

```bash
make up              # docker compose up -d (postgres, redis, kafka, typesense, minio)
make db-bootstrap    # apply db/init/*.sql (roles, schemas)
make migrate         # apply platform.* migrations
make db-verify-rls   # asserts velocity_api has NOBYPASSRLS (ADR-007)
make minio-bucket    # create the velocity-warm bucket (idempotent)
```

Verify with `make ps` (all containers Healthy) and `make db-url`.

To reset everything: `make nuke` wipes containers and `data/` (destructive,
local only).

---

## 3. Bring up the cluster

```bash
make k3d-up
```

Interactive flow:

### Step 1 — Cluster picker

The script lists existing k3d clusters and offers `[n] create a new
cluster`. The default pick is `velocity` if it exists, otherwise the first
listed cluster. You can pin a name (and skip the picker) with
`VELOCITY_CLUSTER=<name>`.

| Situation | Prompt |
|---|---|
| At least one cluster exists | Numbered picker + `[n]` for new |
| No clusters exist | `Name for the new cluster [velocity]:` |
| `VELOCITY_CLUSTER=<name>` pinned, exists | `(u)se existing / (r)ecreate / (n)ew name? [u]` |
| `VELOCITY_CLUSTER=<name>` pinned, missing | `Cluster '<name>' not found. Create it? [Y/n]` |

New clusters are created with host ports **`8080 → 80`** and **`8443 →
443`** mapped through the k3d load balancer (traefik). High host ports
avoid needing root to bind to `<1024`.

### Step 2 — Bring up the release

Once the cluster is up, the script:

1. Brings up the docker-compose stack (postgres/redis/kafka/typesense/minio).
2. Generates a self-signed cert for the webhook into `data/webhook-tls/`.
3. Builds all six velocity images via `docker buildx bake --load`.
4. `k3d image import`s them into the chosen cluster.
5. Applies the CRDs from `crds/` and creates the `velocity-system` namespace.
6. `helm upgrade --install`s the chart with `charts/velocity/values-dev.yaml`
   as the overlay (portal + ingress on, `host.k3d.internal` wired in,
   debug-level logging).

Expected summary line:

```
✓ velocity is up

  Portal:        http://velocity.local:8080/
  API:           http://velocity.local:8080/api   (proxied by the portal nginx)
  Minio console: http://localhost:9001/
```

### Step 3 — Run the acceptance suite

```bash
make e2e
```

`make e2e` asserts the release is present in `velocity-system` and runs
TESTS A–D. With `VELOCITY_E2E_SAMPLES=1` it additionally applies
`samples/*.yaml` (excluding `11-purgerequest.yaml`) after the suite, so
the portal lands on a populated hierarchy ready to demo CRUD.

---

## 4. Non-interactive / CI overrides

`scripts/k3d-up.sh` env vars:

| Variable | Values | Default | Effect |
|---|---|---|---|
| `VELOCITY_CLUSTER` | name | (picker) | Pin cluster, skip picker |
| `VELOCITY_AGENTS` | integer | `0` | Agent (worker) node count beyond control plane |
| `VELOCITY_CLUSTER_ACTION` | `use` \| `recreate` \| `new` | (prompt) | What to do about cluster state |
| `VELOCITY_IMAGE_TAG` | tag | `dev` | Image tag built and referenced |
| `VELOCITY_NAMESPACE` | namespace | `velocity-system` | Helm release namespace |
| `VELOCITY_RELEASE` | name | `velocity` | Helm release name |
| `VELOCITY_HTTP_PORT` | port | `8080` | Host port → cluster :80 (loadbalancer) |
| `VELOCITY_HTTPS_PORT` | port | `8443` | Host port → cluster :443 |
| `VELOCITY_HOST` | hostname | `velocity.local` | Portal Ingress host |
| `VELOCITY_MINIMAL` | `1` | unset | Webhook only; you run the operator out-of-cluster |
| `VELOCITY_SKIP_BUILD` | `1` | unset | Skip `docker buildx bake` (reuse the already-built local images) |
| `VELOCITY_PROGRESS` | `auto` \| `plain` \| `tty` | `auto` | Passed to `docker buildx bake --progress` |

`tests/e2e/run.sh` env vars:

| Variable | Values | Default | Effect |
|---|---|---|---|
| `VELOCITY_E2E_NAMESPACE` | namespace | `velocity-system` | Where to look for the release |
| `VELOCITY_E2E_RELEASE` | name | `velocity` | Release name to assert |
| `VELOCITY_E2E_SAMPLES` | `1` | unset | After the suite, `kubectl apply -f samples/` |

Examples:

```bash
# Skip the picker, always use 'velocity' cluster (create if missing):
VELOCITY_CLUSTER=velocity make k3d-up

# Fast loop: webhook only, no API/portal:
VELOCITY_MINIMAL=1 make k3d-up

# Recreate a fresh cluster with 2 agents on every run (CI):
VELOCITY_CLUSTER=velocity-ci \
VELOCITY_CLUSTER_ACTION=recreate \
VELOCITY_AGENTS=2 \
  make k3d-up

# Iterate without rebuilding all images:
VELOCITY_SKIP_BUILD=1 make k3d-up
```

---

## 5. Operational helpers

```bash
make k3d-logs                       # tail aggregate velocity logs
make k3d-logs COMPONENT=api         # scope to one component
make k3d-status                     # kubectl get all,ingress -n velocity-system
make k3d-psql                       # psql against the host's docker-compose postgres
make k3d-shell                      # sh inside the platform-api pod
make k3d-redeploy                   # rebuild images + helm upgrade + rollout restart
make k3d-clean                      # helm uninstall + delete namespace (keep cluster)
make helm-lint                      # helm lint with the dev overlay
make helm-template                  # render the chart with values-dev.yaml
```

### Tear down

```bash
make k3d-clean                      # release uninstalled, cluster kept
k3d cluster delete velocity         # remove the cluster entirely
```

---

## 6. What each test actually checks

### TEST A — Webhook denies namespace mismatch

Applies a `Domain` named `procurement` with `spec.app=supply-chain` into
namespace `acme-supply` (missing `-chain`). Webhook compares namespace to
`{org}-{app}` and must reject. Asserts both:

- `kubectl apply` exits non-zero.
- Deny message mentions the expected namespace (`acme-supply-chain`).

### TEST B — Webhook admits a valid Domain

Same `Domain`, correct namespace `acme-supply-chain`. Must succeed.

### TEST C — Operator provisions the Postgres schema

The operator's reconciler should:

1. Resolve the effective policy.
2. `CREATE SCHEMA IF NOT EXISTS acme_supply_chain_procurement` plus the
   per-domain roles.

Polls `information_schema.schemata` for up to 60 seconds against the
host's docker-compose Postgres on :5434.

### TEST D — Status reaches Ready

The reconciler writes `status.phase=Ready` once provisioning completes.
Polled for up to 30 seconds.

---

## 7. Troubleshooting

### "kubectl cannot reach a cluster — run 'make k3d-up' first"

`make e2e` requires `make k3d-up` to have been run first. Re-run it.

### Webhook pod CrashLoopBackOff

```bash
kubectl -n velocity-system logs deploy/velocity-webhook
```

Most common cause: stale TLS secret. Wipe and re-run:

```bash
make k3d-clean
rm -rf data/webhook-tls
make k3d-up
```

### TEST C times out — schema never created

The operator is running but not reconciling. Look at the operator logs:

```bash
make k3d-logs COMPONENT=operator
kubectl get domain procurement -n acme-supply-chain -o yaml
```

- Empty `.status` → operator can't reach the API server.
- `.status.phase=Failed` → operator reached Postgres but DDL failed; read
  `.status.message`.

### Stale schema in Postgres

The operator is idempotent, but manual mangling between runs can leave
inconsistent state:

```bash
make psql
DROP SCHEMA acme_supply_chain_procurement CASCADE;
```

Or `make nuke && make dev` for a complete reset.

### k3d image not visible inside cluster

Confirm import succeeded:

```bash
docker exec -it k3d-velocity-server-0 crictl images | grep velocity-webhook
```

If empty, the import didn't reach this node. Re-run `make k3d-up` —
`k3d image import` is idempotent.

### Browser can't resolve velocity.local

```bash
echo '127.0.0.1 velocity.local' | sudo tee -a /etc/hosts
```

---

## 8. CI considerations

`make e2e` is local-only today. Workspace tests (`make test`) cover the
API and operator via `testcontainers` and don't need a k8s cluster.

When we wire e2e into CI (later phase), k3d is the runtime — lighter,
single-digit-second startup, image loading is one command:

```bash
VELOCITY_CLUSTER=velocity-ci \
VELOCITY_CLUSTER_ACTION=recreate \
VELOCITY_AGENTS=1 \
  make k3d-up
make e2e
```

…with `make up && make db-bootstrap && make migrate` as the preceding
job step.
