---
title: Installation
description: Prerequisites, Helm setup, RBAC, and critical database role configuration
---

This guide covers production-grade installation of Velocity on Kubernetes with all critical infrastructure components.

## Prerequisites

### Kubernetes

- Version 1.27 or later
- RBAC enabled (required for SchemaDefinition reconcilers)
- At least 3 nodes with:
  - 2 CPU, 4 GB RAM per node (minimum)
  - 50 GB persistent storage (Postgres)

Verify your cluster:

```sh
kubectl version --short
kubectl get nodes
```

### Postgres

Velocity requires Postgres 15 or later with:
- SSL/TLS enabled
- Row-level security (RLS) enabled (default in Postgres 9.5+)
- A superuser account for initial setup (operator uses it to create roles, then switches to non-superuser)

You can use:
- A managed service (AWS RDS, Google Cloud SQL, Azure Database for Postgres)
- CloudNativePG (recommended for HA; operator-managed Postgres on k8s)
- An existing Postgres cluster

### Redis

Optional but strongly recommended for production. Used for:
- Actor revocation list (deny-by-default auth fail mode per ADR-003)
- Idempotency key storage
- Session tokens

- Single instance for development
- Redis Sentinel or Cluster for production HA

### Typesense (Optional)

Required only if you plan to use Tier-3 search (real-time, typo-tolerant, full-text). Skip this if Tier 1 (trigram) or Tier 2 (Postgres FTS) is sufficient.

- Typesense 0.25+ (cloud or self-hosted)
- Network access from velocity-api pods

### Kafka (Optional)

Used for:
- Hooks (webhook delivery on mutations)
- Long-term event replay

Skip if you don't need webhooks or external event streams. A future ADR will clarify when Kafka is mandatory vs optional.

### Other Tools

- `helm` 3.12+
- `kubectl` configured to your cluster
- `velocity` CLI (installed post-Helm — see below)

## Install the `velocity` CLI

Pick one. The CLI is a single statically-linked binary; all install paths produce the same artefact.

**Homebrew (macOS / Linux):**

```sh
brew tap kinarix/velocity     # one-time
brew install velocity
velocity --version
```

If the tap isn't published yet, install the formula by raw URL:

```sh
brew install --formula \
  https://raw.githubusercontent.com/kinarix/velocity/main/Formula/velocity.rb
```

**Curl install (Linux / macOS):**

```sh
curl -fsSL https://raw.githubusercontent.com/kinarix/velocity/main/scripts/install.sh | sh
```

Installs to `/usr/local/bin/velocity` when writable, falling back to `$HOME/.local/bin`. Verifies the SHA-256 of the downloaded tarball against the matching `.sha256` file in the GitHub Release.

**Direct download:**

Grab the right tarball for your platform from [github.com/kinarix/velocity/releases/latest](https://github.com/kinarix/velocity/releases/latest). Targets shipped per release:

- `velocity-vX.Y.Z-aarch64-apple-darwin.tar.gz`     — Apple Silicon
- `velocity-vX.Y.Z-x86_64-apple-darwin.tar.gz`      — Intel Mac
- `velocity-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` — Linux arm64 (static)
- `velocity-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz`  — Linux x86_64 (static)

Each is paired with a `.sha256` file. Verify before extracting.

## Container images

Server-side components run as containers. The release workflow publishes seven images to `ghcr.io/kinarix/<bin>`:

| Image | Role |
|---|---|
| `ghcr.io/kinarix/velocity-api` | Axum REST API server |
| `ghcr.io/kinarix/velocity-operator` | kube-rs reconcilers |
| `ghcr.io/kinarix/velocity-webhook` | Validating admission webhook |
| `ghcr.io/kinarix/velocity-archive-worker` | Archive batch worker |
| `ghcr.io/kinarix/velocity-log-collector` | DaemonSet log shipper |
| `ghcr.io/kinarix/velocity-log-processor` | Log enrichment / filter / route |
| `ghcr.io/kinarix/velocity-warm-reader` | Warm-tier DataFusion reader |

Each image is multi-arch (`linux/amd64` + `linux/arm64`). Tag scheme:

- `:X.Y.Z` — exact release
- `:X.Y` — latest patch in a minor line
- `:latest` — most recent release (production deployments should pin)
- `:main`, `:sha-<short>` — every push to `main` for forward testing

The Helm chart references these images by digest in production; `values-dev.yaml` uses `:latest` so a `helm upgrade` picks up the newest main build without a chart bump.

## Step 1: Add Helm Repository

The chart is published two ways. Pick whichever fits your tooling — the produced release is identical.

**Classic repo (recommended for most users):**

```sh
helm repo add velocity https://velocity.kinarix.com/charts
helm repo update
helm search repo velocity
```

**OCI artifact on `ghcr.io`:**

```sh
# No `helm repo add` step — install directly by URL.
helm install velocity \
  oci://ghcr.io/kinarix/charts/velocity \
  --version 0.1.0 \
  -n velocity-system --create-namespace
```

Both flavours are published by [`helm-publish.yml`](https://github.com/kinarix/velocity/blob/main/.github/workflows/helm-publish.yml) on tags of shape `chart-v<semver>`. The classic repo is served from the same GitHub Pages deployment as this documentation site, so `index.yaml` and the `.tgz` files live at `https://velocity.kinarix.com/charts/`.

## Step 2: Create Namespace and Secrets

```sh
kubectl create namespace velocity-system
```

### Postgres credentials secret

```sh
kubectl create secret generic velocity-postgres-creds \
  --from-literal=superuser-password='<strong-password>' \
  --from-literal=velocity-api-password='<strong-password>' \
  --from-literal=velocity-operator-password='<strong-password>' \
  -n velocity-system
```

The operator will use the superuser account to create the non-superuser `velocity_api` role.

### Redis credentials secret (if using Redis)

```sh
kubectl create secret generic velocity-redis-creds \
  --from-literal=password='<redis-password>' \
  -n velocity-system
```

### S3 credentials secret (if using S3 for warm-tier archive storage)

```sh
kubectl create secret generic velocity-s3-creds \
  --from-literal=access-key-id='<aws-access-key>' \
  --from-literal=secret-access-key='<aws-secret-key>' \
  -n velocity-system
```

## Step 3: Create values.yaml

```yaml
# values.yaml for Velocity Helm chart

global:
  org: acme  # Your organization name (used in schema paths)
  domain: velocity.acme.com

postgres:
  # If using CloudNativePG (recommended), set enabled: true
  cnpg:
    enabled: true
    instances: 3  # Primary + 2 replicas
    storage: 50Gi
    
  # If using external Postgres:
  external:
    enabled: false
    host: postgres.example.com
    port: 5432
    database: velocity
    superuserSecret:
      name: velocity-postgres-creds
      superuserKey: superuser-password
    
  backup:
    enabled: true
    s3:
      bucket: velocity-backups
      region: us-east-1
      credentialsSecret:
        name: velocity-s3-creds

redis:
  enabled: true
  sentinelEnabled: true  # Use Redis Sentinel for HA
  replicas: 3
  credentialsSecret:
    name: velocity-redis-creds

typesense:
  enabled: true
  endpoint: https://typesense.example.com:8108
  apiKey: <your-typesense-api-key>

kafka:
  enabled: false  # Set to true if using webhooks
  brokers: kafka-0:9092,kafka-1:9092,kafka-2:9092
  replicationFactor: 3

# API server configuration
api:
  replicas: 2
  autoscaling:
    enabled: true
    minReplicas: 2
    maxReplicas: 10
    targetCPUUtilizationPercentage: 70
  
  image:
    repository: velocity/velocity-api
    tag: v0.9.0
  
  resources:
    requests:
      cpu: 500m
      memory: 512Mi
    limits:
      cpu: 2000m
      memory: 2Gi

# Operator configuration
operator:
  replicas: 1  # Leader-elected; only 1 active
  image:
    repository: velocity/velocity-operator
    tag: v0.9.0
  
  resources:
    requests:
      cpu: 200m
      memory: 256Mi
    limits:
      cpu: 1000m
      memory: 1Gi

# Validating webhook configuration
webhook:
  replicas: 3
  image:
    repository: velocity/velocity-webhook
    tag: v0.9.0
  
  failurePolicy: Fail  # Block applies on webhook error (safe for most CRDs)
  timeoutSeconds: 5
  
  resources:
    requests:
      cpu: 100m
      memory: 128Mi
    limits:
      cpu: 500m
      memory: 512Mi

# Archive worker (Phase 8+)
archiveWorker:
  enabled: true
  replicas: 1
  image:
    repository: velocity/velocity-archive-worker
    tag: v0.9.0

# Observability
observability:
  prometheus:
    enabled: true
    scrapeInterval: 30s
  
  otel:
    enabled: true
    endpoint: otel-collector.monitoring:4318

# Security
security:
  podSecurityPolicy: restricted
  networkPolicy:
    enabled: true
  
  # TLS for internal communication
  tls:
    enabled: true
    issuer: letsencrypt-prod  # cert-manager issuer
```

## Step 4: Install the Helm Chart

```sh
helm install velocity velocity/velocity \
  --namespace velocity-system \
  --values values.yaml
```

Wait for rollout:

```sh
kubectl rollout status deployment/velocity-api -n velocity-system
kubectl rollout status deployment/velocity-operator -n velocity-system
kubectl rollout status deployment/velocity-webhook -n velocity-system
```

Verify pods are running:

```sh
kubectl get pods -n velocity-system
```

All should show `Running` and ready `1/1`.

## Step 5: Critical: Verify Non-Superuser Database Role

The operator creates a non-superuser role called `velocity_api` with `NOBYPASSRLS=true`. This is load-bearing for security (ADR-007).

### Verify the role exists and has correct settings

```sh
psql -h <postgres-host> -U <superuser> -d velocity -c \
  "SELECT rolname, rolbypassrls, rolinherit FROM pg_roles WHERE rolname = 'velocity_api';"
```

Output must show:

```
 rolname    | rolbypassrls | rolinherit
────────────┼──────────────┼───────────
 velocity_api|     f        |     t
```

- `rolbypassrls = f` (false) means RLS is enforced for this role
- `rolinherit = t` (true) means the role inherits permissions from parent roles

If `rolbypassrls = t`, row-level security is bypassed and the deployment is **not secure**. The API server startup will refuse to start and log:

```
FATAL velocity_api role has BYPASSRLS=true — RLS will not work. Fix the role.
```

### If the role is misconfigured

Fix it as the superuser:

```sql
ALTER ROLE velocity_api NOBYPASSRLS;
```

Then restart the velocity-api deployment:

```sh
kubectl rollout restart deployment/velocity-api -n velocity-system
```

## Step 6: Create Platform Migrations

The operator applies platform-level migrations on startup. Verify they succeeded:

```sh
psql -h <postgres-host> -U velocity_api -d velocity -c \
  "SELECT table_name FROM information_schema.tables WHERE table_schema = 'platform';"
```

Expected tables:

```
platform.schema_definitions
platform.field_definitions
platform.event_log
platform.audit_log
platform.audit_chain_state
platform.audit_insert (stored procedure)
platform.api_keys
platform.role_bindings
platform.sessions
platform.idempotency_keys
platform.archive_runs
platform.purge_requests
```

## Step 7: Set Up RBAC (Optional but Recommended)

Create a ClusterRole for operators:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: velocity-operator
rules:
  - apiGroups: ["velocity.sh"]
    resources: ["organisations", "applications", "domains", "schemadefinitions", "authstrategies", "archivepolicies", "purgerequests", "rolebindings"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["velocity.sh"]
    resources: ["schemadefinitions/status"]
    verbs: ["get", "patch", "update"]
  - apiGroups: [""]
    resources: ["secrets", "configmaps"]
    verbs: ["get", "list", "watch"]
  - apiGroups: ["apps"]
    resources: ["deployments", "statefulsets"]
    verbs: ["get", "list", "watch", "create", "update", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: velocity-operator
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: velocity-operator
subjects:
  - kind: ServiceAccount
    name: velocity-operator
    namespace: velocity-system
```

Apply:

```sh
kubectl apply -f rbac.yaml
```

## Step 8: Install the velocity CLI

Download the latest release:

```sh
curl -L https://releases.velocity.sh/velocity-latest-linux-x64 \
  -o /usr/local/bin/velocity
chmod +x /usr/local/bin/velocity
```

Or install via package manager (if available):

```sh
# macOS
brew install velocity

# Ubuntu/Debian (apt)
sudo apt-add-repository ppa:velocity/stable
sudo apt install velocity

# RHEL/CentOS (yum)
sudo yum install velocity
```

Verify installation:

```sh
velocity version
```

## Step 9: Configure CLI Context

Set up the CLI to point to your API server:

```sh
velocity context add \
  --name prod \
  --api-url https://api.velocity.acme.com \
  --bearer-token <jwt-or-api-key>
```

Or if using service account token:

```sh
TOKEN=$(kubectl create token velocity-api-client -n velocity-system)
velocity context add \
  --name prod \
  --api-url https://api.velocity.acme.com \
  --bearer-token $TOKEN
```

Make it the default:

```sh
velocity context use prod
```

Test:

```sh
velocity version
velocity health
```

## Step 10: Verify All Components

```sh
kubectl get deployment,statefulset,pod -n velocity-system
```

Expected services:
- `velocity-api` (2+ replicas)
- `velocity-operator` (1 replica, leader-elected)
- `velocity-webhook` (3 replicas)
- `velocity-archive-worker` (1 replica, if Phase 8+ enabled)

Check logs for errors:

```sh
kubectl logs -n velocity-system -l app=velocity-api --tail=50 -f
kubectl logs -n velocity-system -l app=velocity-operator --tail=50 -f
```

All should show JSON-formatted logs with no FATAL errors.

## Production Hardening

For production deployments, also see:

- **[Hardening](./hardening)** — Security checklist, CEL sandbox verification, input size limits, audit trail requirements.
- **[Security](./security)** — Threat model, RLS enforcement verification, auth fail modes, validating webhook importance.
- **[Troubleshooting](./troubleshooting)** — Common issues and recovery procedures.

## Next Steps

1. [Getting Started](./getting-started) — Apply your first SchemaDefinition and CRUD a record.
2. [Hardening](./hardening) — Complete the production security checklist.
3. [API Reference](./api-reference) — Understand the REST API surface.
4. [Operations](./runbooks) — Set up backup, restore, and failover procedures.
