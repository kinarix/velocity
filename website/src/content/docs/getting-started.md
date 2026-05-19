---
title: Getting Started
description: Install Velocity and apply your first SchemaDefinition
---

This guide takes you from zero to creating, reading, and querying records in 15 minutes.

## Prerequisites

- Kubernetes cluster (1.27+)
- Postgres 15+ (or use CloudNativePG)
- `kubectl` configured
- `helm` 3.12+
- `velocity` CLI (installed in the next step)

## Step 1: Install the Platform

### Add Helm repository

```sh
helm repo add velocity https://charts.velocity.sh
helm repo update
```

### Install velocity-operator, velocity-api, and validating-webhook

```sh
helm install velocity velocity/velocity \
  --namespace velocity-system \
  --create-namespace \
  --values values.yaml
```

Where `values.yaml` contains:

```yaml
postgres:
  host: postgres.example.com
  port: 5432
  database: velocity
  username: velocity_admin
  password: <secret>

redis:
  host: redis.example.com
  port: 6379

api:
  replicas: 2
  image:
    tag: v0.9.0

operator:
  replicas: 1
  image:
    tag: v0.9.0

webhook:
  replicas: 3
  image:
    tag: v0.9.0
```

Wait for readiness:

```sh
kubectl rollout status deployment/velocity-api -n velocity-system
```

### Install the velocity CLI

```sh
curl -L https://releases.velocity.sh/velocity-v0.9.0-linux-x64 -o /usr/local/bin/velocity
chmod +x /usr/local/bin/velocity
velocity version
```

## Step 2: Create Your First SchemaDefinition

Apply the following YAML to your cluster:

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: purchase-order
  namespace: acme-supply-chain-procurement
  labels:
    velocity.sh/org: acme
    velocity.sh/app: supply-chain
    velocity.sh/domain: procurement
    velocity.sh/version: v1
spec:
  org: acme
  app: supply-chain
  domain: procurement
  object: purchase-order
  version: v1
  
  fields:
    - name: id
      type: string
      required: true
      unique: true
      description: "PO identifier (PO-XXXXXXXX)"
    
    - name: supplier_code
      type: string
      required: true
      description: "Supplier identifier"
      filterable: true
    
    - name: amount
      type: number
      required: true
      description: "PO amount in USD"
      filterable: true
      sortable: true
    
    - name: status
      type: enum
      enum: [draft, approved, shipped, delivered, archived]
      required: true
      default: draft
      filterable: true
      sortable: true
    
    - name: created_at
      type: timestamp
      autoPopulated: true
      filterable: true
      sortable: true
    
    - name: created_by
      type: string
      autoPopulated: true
      description: "Actor who created the PO"
    
    - name: notes
      type: string
      required: false
      maxLength: 1000
  
  uniqueConstraints:
    - fields: [id]
  
  validation:
    rules:
      - rule: "self.amount > 0"
        message: "PO amount must be positive"
  
  search:
    tier: 2  # Postgres FTS
    fields: [supplier_code, notes]
  
  access:
    roles:
      create: [procurement-writer]
      read: [procurement-reader, procurement-writer]
      update: [procurement-writer]
      delete: [procurement-admin]
  
  timeMachine:
    enabled: true
    hotRetention: 90d
  
  archive:
    enabled: false
```

Save this as `purchase-order-schema.yaml` and apply:

```sh
kubectl apply -f purchase-order-schema.yaml
```

Verify the schema is ready:

```sh
kubectl get schemadef purchase-order -n acme-supply-chain-procurement
```

Output should show:

```
NAME               STATUS   ORG    APP            DOMAIN        OBJECT           VERSION
purchase-order    Ready    acme   supply-chain   procurement   purchase-order   v1
```

## Step 3: Wire the CLI Context

The `velocity` CLI needs to know which API server and credentials to use.

```sh
velocity context add \
  --name acme-prod \
  --api-url https://api.velocity.acme.com \
  --bearer-token <your-jwt-or-api-key>
```

Make it the default:

```sh
velocity context use acme-prod
```

Verify connectivity:

```sh
velocity version
```

Output:

```
Client: v0.9.0 (built 2026-05-19)
Server: v0.9.0 (built 2026-05-18)
API: https://api.velocity.acme.com
Context: acme-prod
```

## Step 4: Create a Record

Create a purchase order via the REST API or the CLI:

### Via velocity CLI

```sh
velocity record create \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --data '{
    "id": "PO-00000001",
    "supplier_code": "TATA001",
    "amount": 50000.00,
    "status": "draft",
    "notes": "Office supplies for Q2"
  }'
```

Output:

```json
{
  "id": "PO-00000001",
  "supplier_code": "TATA001",
  "amount": 50000.00,
  "status": "draft",
  "notes": "Office supplies for Q2",
  "created_at": "2026-05-19T14:32:15Z",
  "created_by": "ravi.kumar",
  "version": 1
}
```

### Via REST API (curl)

```sh
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "PO-00000001",
    "supplier_code": "TATA001",
    "amount": 50000.00,
    "status": "draft",
    "notes": "Office supplies for Q2"
  }'
```

## Step 5: Read the Record

```sh
velocity record get \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --id PO-00000001
```

Output: Same record with `version: 1`.

## Step 6: Update the Record

```sh
velocity record update \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --id PO-00000001 \
  --data '{"status": "approved"}' \
  --version 1
```

The `--version` flag enforces optimistic locking. If someone else updated the record, you will get a 409 Conflict error. Retry with the new version.

## Step 7: Query Records

List all POs:

```sh
velocity record list \
  --schema acme/supply-chain/procurement/purchase-order/v1
```

Query with filtering:

```sh
velocity record query \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --filter 'status=approved' \
  --sort 'amount:desc' \
  --limit 10
```

## Step 8: View History

Every change is recorded in the time-machine history. View changes to a record:

```sh
velocity history list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --id PO-00000001
```

Output:

```
TIMESTAMP              ACTOR        OPERATION VERSION OLD_VALUE NEW_VALUE
2026-05-19 14:32:15  ravi.kumar   CREATE    1       -         {"status":"draft",...}
2026-05-19 14:35:22  ravi.kumar   UPDATE    2       {"status":"draft"}  {"status":"approved"}
```

Diff two points in time:

```sh
velocity history diff \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --id PO-00000001 \
  --from 2026-05-19T14:32:15Z \
  --to 2026-05-19T14:35:22Z
```

Restore to a past state:

```sh
velocity restore \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --id PO-00000001 \
  --at 2026-05-19T14:32:15Z \
  --reason "Revert approval per stakeholder request"
```

This writes a new event (not a rollback) that restores the old state.

## Step 9: Verify Audit Chain

Every mutation is recorded in an append-only audit log with hash-linked integrity. Verify the chain:

```sh
velocity audit verify \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --id PO-00000001
```

Output:

```
✓ Audit chain valid (5 events, 0 tampering detected)
```

## Step 10: Check Platform Health

```sh
velocity health
```

Output:

```
Component        Status   Latency
─────────────────────────────────
Postgres         OK       4ms
Redis            OK       2ms
Typesense        OK       15ms
Kafka            OK       8ms
```

## What's Next?

- **[Installation](./installation)** — Production-grade setup with HA, backups, and security hardening.
- **[Hardening](./hardening)** — Security checklist before deploying to production.
- **[Schema Definition](./features/schema-definition)** — Explore all field types, constraints, and validation rules.
- **[Authentication](./features/auth)** — Set up JWT, OIDC, API keys, and composite strategies.
- **[API Reference](./api-reference)** — Complete REST endpoint documentation.
- **[CLI Reference](./features/cli)** — Every command with examples.
- **[Troubleshooting](./troubleshooting)** — Common issues and fixes.

You now have a running Velocity deployment. Congratulations!
