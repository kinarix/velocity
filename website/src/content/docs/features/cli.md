---
title: CLI Reference
description: velocity CLI — every command grouped by implementation phase
---

The `velocity` CLI is the operator's interface for schema management, authentication, archive operations, and troubleshooting. Commands are grouped by implementation phase (Phase 4.5, Phase 8, Phase 9).

## Installation

```bash
# Via Helm (comes with velocity-api pod)
kubectl exec -n velocity-system deployment/velocity-api -- velocity --version

# Or install locally
cargo install --path ./crates/velocity-cli
# or
brew install velocity  # when published to Homebrew

velocity --version
# velocity 0.1.0
```

## Global Options

All commands accept:

```
--config FILE          Config file path (default: ~/.velocity/config.yaml)
--context STRING       Named context (default: current-context from config)
--org STRING           Override org from context
--output FORMAT        json | yaml | table (default: table)
--quiet                Suppress progress output
--debug                Enable debug logging
```

## Context Management (Phase 4.5)

### velocity context list

List configured contexts:

```bash
velocity context list
```

Output:

```
NAME        CLUSTER             ORG     APP
dev         localhost:6443      acme    supply-chain
prod        api.acme.com:443    acme    supply-chain
staging     staging.acme.com    acme    supply-chain
```

### velocity context add

Register a new context:

```bash
velocity context add \
  --name prod \
  --server https://api.acme.com \
  --org acme \
  --app supply-chain \
  --ca-file ~/.kube/ca.crt
```

Config is stored in `~/.velocity/config.yaml`:

```yaml
contexts:
  prod:
    server: https://api.acme.com
    org: acme
    app: supply-chain
    ca_file: ~/.kube/ca.crt
current_context: prod
```

### velocity context use

Switch active context:

```bash
velocity context use prod
# Context switched to 'prod'
```

## Authentication (Phase 4.5)

### velocity auth login

Obtain a token and store it:

```bash
velocity auth login --strategy jwt
# Opens browser to auth endpoint
# Returns access token, stores in ~/.velocity/token
```

OIDC flow:

```bash
velocity auth login --strategy oidc
# 1. Opens https://idp.example.com/authorize?...
# 2. You authenticate and consent
# 3. Redirects back with authorization code
# 4. CLI exchanges code for token and stores locally
```

### velocity auth logout

Clear stored token:

```bash
velocity auth logout
# Token cleared from ~/.velocity/token
```

### velocity api-key create

Create an API key for CI/CD (Phase 8 slice 5):

```bash
velocity api-key create \
  --name deploy-service \
  --ttl 90d \
  --scope region=west
```

Output:

```
vel_deploy-service_abc123def456xyz...
SAVE THIS NOW — you will not see it again.
```

**Key point:** The CLI never stores or retrieves the plaintext key. You MUST save the output; it is SHA256 hashed in the database immediately.

Use in deploy scripts:

```bash
curl -H "X-API-Key: vel_deploy-service_abc123def456xyz..." \
  https://api.velocity.acme.com/api/acme/supply-chain/...
```

### velocity api-key list

List created keys (shows only metadata, not plaintext):

```bash
velocity api-key list
```

Output:

```
NAME                   EXPIRES                 CREATED                 LAST_USED
deploy-service         2026-08-19T00:00:00Z    2026-05-19T14:32:00Z    2026-05-19T15:45:00Z
ci-integration         2026-06-30T00:00:00Z    2026-05-01T10:00:00Z    never
```

### velocity api-key revoke

Revoke a key immediately:

```bash
velocity api-key revoke --name ci-integration
# Key revoked. Existing requests using this key will be denied within seconds.
```

## Access Control (Phase 4.5)

### velocity grant

Grant roles to an actor:

```bash
velocity grant \
  --actor ravi.kumar \
  --roles procurement-reader,procurement-writer \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --scope region=west,store_ids=10:20:30 \
  --expires 2027-12-31
```

Verifies:

```bash
velocity grant \
  --actor ravi.kumar \
  --schema acme/supply-chain/procurement/purchase-order/v1
```

Output:

```
procurement-reader    ✓ (expires 2027-12-31)
procurement-writer    ✓ (expires 2027-12-31)
Scope: region=west, store_ids=[10,20,30]
```

### velocity revoke

Revoke all roles from an actor on a schema:

```bash
velocity revoke \
  --actor ravi.kumar \
  --schema acme/supply-chain/procurement/purchase-order/v1
```

Revocation is immediate (broadcast to Redis within seconds).

### velocity role list

List available roles for a schema:

```bash
velocity role list --schema acme/supply-chain/procurement/purchase-order/v1
```

Output:

```
procurement-reader      create, read
procurement-writer      create, read, update, delete
procurement-admin       create, read, update, delete, restore, audit-read
```

## Schema Management (Phase 4.5 onward)

### velocity schema apply

Apply a SchemaDefinition (uses `kubectl apply` internally):

```bash
velocity schema apply --file purchase-order-schema.yaml
```

Output:

```
purchase-order created (PurchaseOrder.acme/supply-chain/procurement/purchase-order/v1)
Postgres table: acme_supply_chain_procurement.purchase_order_v1
Status: Ready
```

Idempotent; applying the same schema twice is safe.

### velocity schema list

List all schemas in an org/app:

```bash
velocity schema list --org acme --app supply-chain
```

Output:

```
SCHEMA                                                    VERSION    STATUS    RECORDS
acme/supply-chain/procurement/purchase-order              v1         Ready     15042
acme/supply-chain/procurement/requisition                 v1         Ready     3201
acme/supply-chain/sourcing/supplier                       v2         Ready     847
acme/supply-chain/sourcing/contract                       v1         Pending   0
```

### velocity schema get

Inspect a schema definition:

```bash
velocity schema get acme/supply-chain/procurement/purchase-order/v1 --output yaml
```

Output:

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: purchase-order
  namespace: acme-supply-chain-procurement
spec:
  description: "Purchase orders and requisitions"
  fields:
    - name: id
      type: string
      ...
  search:
    tier: 3
    fields:
      - name: supplier_code
        searchable: true
        facet: true
  timeMachine:
    enabled: true
    hotRetention: 90d
    warmRetention: 5y
```

### velocity schema validate

Validate schema YAML without applying (dry-run):

```bash
velocity schema validate --file purchase-order-schema.yaml
```

Output:

```
✓ purchase-order schema is valid
  Fields: 12 (all constraints valid)
  CEL expressions: 3 (all compile-time valid)
  Auth policies: 2 (all RBAC role names exist)
  Archive: age trigger configured (90 days)
  Search: Typesense Tier 3 (fields: supplier_code, notes)
```

## Query & Data Operations (Phase 4.5 onward)

### velocity query

Execute a query against a schema:

```bash
velocity query \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --select id,supplier_code,amount,status \
  --where 'status = "approved"' \
  --order-by amount:desc \
  --limit 10
```

Builds and executes the query endpoint internally. Output:

```
ID           SUPPLIER_CODE    AMOUNT       STATUS
PO-00000001  TATA001          50000        approved
PO-00000002  ACC001           60000        approved
PO-00000005  TATA_INC         45000        approved
```

JSON output:

```bash
velocity query --schema ... --output json | jq '.data[] | {id, amount}'
```

## Audit (Phase 4.5 onward)

### velocity audit list

List audit events for a schema or entity:

```bash
velocity audit list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001 \
  --limit 50
```

Output:

```
EVENT_ID    TIMESTAMP                ACTOR          OPERATION    REASON
1           2026-05-19T14:32:00Z     ravi.kumar     CREATE       (none)
2           2026-05-19T14:35:00Z     anita.sharma   UPDATE       Approved per stakeholder request
3           2026-05-19T14:40:00Z     system         DELETE       TTL purge
```

By actor and time range:

```bash
velocity audit list \
  --actor ravi.kumar \
  --since 7d \
  --operation UPDATE,DELETE
```

### velocity audit verify

Verify audit chain integrity (hash-linked):

```bash
velocity audit verify \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001
```

Output:

```
✓ Audit chain valid (42 events, 0 tampering detected)
  Last hash: abc123def456...
  Chain integrity: verified
```

If tampering detected:

```
✗ Audit chain invalid (hash mismatch at event 15)
  Event 14 hash: abc123... (✓)
  Event 15 hash: def456... expected abc789...
  Events 15-42 are suspect
  Action required: Investigate source, contact security team
```

## Time Machine (Phase 4.5 onward)

### velocity history list

List all changes to a record:

```bash
velocity history list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001
```

Output:

```
EVENT_ID    TIMESTAMP                OPERATION    ACTOR
1           2026-05-19T14:32:00Z     CREATE       ravi.kumar
2           2026-05-19T14:35:00Z     UPDATE       anita.sharma
3           2026-05-19T14:40:00Z     RESTORE      ravi.kumar
```

### velocity history at

Query a record at a specific point in time:

```bash
velocity history at \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001 \
  --timestamp "2026-05-19T14:33:00Z"
```

Output:

```
id: PO-001
status: draft
amount: 50000
created_at: 2026-05-19T14:32:00Z
version: 1
```

### velocity history diff

Compare record state between two timestamps:

```bash
velocity history diff \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001 \
  --from "2026-05-19T14:32:00Z" \
  --to "2026-05-19T14:35:00Z"
```

Output:

```
CHANGED
  status: draft → approved
  approval_date: (none) → 2026-05-19T14:35:00Z
ADDED
  (none)
REMOVED
  (none)
```

### velocity restore

Restore a record to a past state (Phase 8 slice 7):

```bash
velocity restore \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001 \
  --at "2026-05-19T14:33:00Z" \
  --reason "Approval was incorrect; customer dispute"
```

Output:

```
✓ Restored PO-001 to 2026-05-19T14:33:00Z
  New version: 3
  Restored at: 2026-05-19T14:40:00Z
  Reason logged in audit trail
```

## Archive & Purge (Phase 8 onward)

### velocity archive apply

Create an ArchivePolicy (using kubectl apply):

```bash
velocity archive apply --file archive-policy.yaml
```

### velocity archive list

List archive policies for a schema:

```bash
velocity archive list --schema acme/supply-chain/procurement/purchase-order/v1
```

Output:

```
POLICY         TRIGGER    SCHEDULE      STATUS      LAST_RUN
po-archive     age:90d    0 2 * * *     Success     2026-05-19T02:15:00Z
```

### velocity archive query

Query archived records (Phase 8 slice 10):

```bash
velocity archive query \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --where 'status = "delivered" AND created_at < "2026-02-01"' \
  --select id,supplier_code,amount \
  --limit 100
```

Output:

```
FILES_SCANNED    ROWS_EXAMINED    ELAPSED_MS    COUNT
3                125000           234           87
```

### velocity unarchive

Restore an archived record to hot storage (Phase 8 slice 10):

```bash
velocity unarchive \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001 \
  --reason "Customer dispute; needs investigation"
```

Output:

```
✓ Unarchived PO-00000001
  Restored to hot tier: acme_supply_chain_procurement.purchase_order_v1
  Restored at: 2026-05-19T14:40:00Z
```

### velocity purge apply

Create a PurgeRequest (Phase 8 slice 7):

```bash
velocity purge apply --file purge-request.yaml
```

The request is held in `Pending` state until approved.

### velocity purge list

List pending and completed purge requests:

```bash
velocity purge list --schema acme/supply-chain/procurement/purchase-order/v1
```

Output:

```
REQUEST              CREATED                 STATUS       ESTIMATED_RECORDS
q1-2024-purge        2026-05-19T14:00:00Z    Pending      125000
q4-2023-purge        2026-04-19T10:30:00Z    Approved     98000
```

### velocity purge approve

Approve a purge request (Phase 8 slice 7):

```bash
velocity purge approve \
  --purge-request q1-2024-purge \
  --reason "Approved by compliance team per retention policy"
```

Output:

```
✓ Purge request q1-2024-purge approved
  Estimated records to delete: 125000
  Archive worker will begin deletion
```

## Debugging & Diagnostics

### velocity status

Check cluster and operator health:

```bash
velocity status
```

Output:

```
API Server
  Status: Ready
  Version: 0.1.0
  Replicas: 3/3
  Uptime: 45d 3h

Operator
  Status: Ready
  Version: 0.1.0
  Replicas: 1/1
  Reconcile rate: 0.12/sec

Registry
  Schemas loaded: 147
  Last sync: 5s ago
  Sync duration: 823ms

Database
  Postgres: Ready (15 connections)
  Redis: Ready
  Typesense: Ready (3 collections)

Webhooks
  ValidatingWebhook: Ready
  Mutations rejected (last 24h): 7
```

### velocity logs

Tail API or operator logs:

```bash
velocity logs --component api --tail 100
velocity logs --component operator --tail 50 --follow
```

### velocity metrics

Export metrics for manual inspection:

```bash
velocity metrics
# Queries /metrics and pretty-prints Prometheus format
```

## Search Management (Phase 4.5+ for Tiers 1-2, Phase 8 for Tier 3)

### velocity search reindex

Manually trigger a search index rebuild (Phase 10, currently manual via kubectl):

```bash
# Deferred — use kubectl to trigger manually
kubectl patch SchemaDefinition purchase-order -p '{"metadata":{"annotations":{"velocity.sh/reindex":"true"}}}'
```

## Help & Version

### velocity --version

Show CLI version:

```bash
velocity --version
# velocity 0.1.0
```

### velocity --help

Show all commands:

```bash
velocity --help
```

### velocity COMMAND --help

Show help for a specific command:

```bash
velocity schema apply --help
velocity history diff --help
```

## Configuration File

Default location: `~/.velocity/config.yaml`

```yaml
contexts:
  dev:
    server: http://localhost:8080
    org: acme
    app: supply-chain
    ca_file: null
    token_file: ~/.velocity/token.dev
  
  prod:
    server: https://api.acme.com
    org: acme
    app: supply-chain
    ca_file: ~/.kube/ca.crt
    token_file: ~/.velocity/token.prod

current_context: dev

# Global defaults
output_format: table
```

## Examples

### Example 1: Create a record and verify audit trail

```bash
# Apply schema
velocity schema apply --file purchase-order.yaml

# Create a record (via API, then verify via CLI)
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"PO-00000001","supplier_code":"TATA001","amount":50000}'

# View audit trail
velocity audit list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001

# Verify chain integrity
velocity audit verify \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001
```

### Example 2: Restore a record to an earlier state

```bash
# List history
velocity history list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001

# Check state at specific time
velocity history at \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001 \
  --timestamp "2026-05-19T14:30:00Z"

# Restore
velocity restore \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001 \
  --at "2026-05-19T14:30:00Z" \
  --reason "Reverting unapproved change"

# Verify new RESTORE event in audit trail
velocity audit list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-00000001 \
  --limit 5
```

### Example 3: Create and manage API keys for CI/CD

```bash
# Create key
velocity api-key create --name ci-deploy --ttl 90d

# List keys
velocity api-key list

# Use in GitHub Actions
# (store plaintext in GitHub Secrets, never commit)
curl -H "X-API-Key: $VELOCITY_API_KEY" https://api.acme.com/...

# After 90 days, revoke old and create new
velocity api-key revoke --name ci-deploy
velocity api-key create --name ci-deploy-new --ttl 90d
```

## Roadmap (Deferred Features)

- **Phase 10:** `velocity schema diff` (compare two schema versions)
- **Phase 10:** `velocity export` (bulk export to CSV/JSON)
- **Phase 10:** `velocity webhook test` (validate webhook configuration before deploy)
- **Phase 11:** `velocity search reindex` (manual trigger via CLI, not annotation)
- **Phase 11:** Interactive mode (REPL for exploratory queries)

