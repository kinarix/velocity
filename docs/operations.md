# Velocity — Operations

> Backup, restore, disaster recovery, failover, upgrades, secrets rotation.
> Everything the platform team needs to keep Velocity running in production.

---

## 1. Backup Strategy

### Postgres

Velocity uses CloudNativePG (CNPG) for Postgres operations. CNPG handles:

- **Continuous WAL archival** to S3 (every WAL segment, ~16MB)
- **Periodic base backups** (daily by default, configurable)
- **Point-in-time recovery (PITR)** to any timestamp within the retention window

Configuration:

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: velocity-postgres
spec:
  instances: 3                          # primary + 2 replicas
  
  backup:
    barmanObjectStore:
      destinationPath: s3://velocity-backups/postgres
      s3Credentials:
        accessKeyId:
          name:  backup-creds
          key:   ACCESS_KEY_ID
        secretAccessKey:
          name:  backup-creds
          key:   SECRET_ACCESS_KEY
      wal:
        compression: gzip
        encryption:  AES256
      data:
        compression: gzip
        encryption:  AES256
    retentionPolicy: "30d"              # 30 days of base backups
    
  bootstrap:
    initdb:
      database: velocity
      owner:    velocity_admin
```

**Recovery targets:**
- **RPO** (Recovery Point Objective): 5 minutes — WAL is archived every 5 min
- **RTO** (Recovery Time Objective): 15 minutes — depends on backup size and S3 throughput

### Object storage (S3) — for warm/cold time machine

S3 versioning enabled on the bucket. Lifecycle rules:

```
0-30 days:   Standard
31-365 days: Standard-IA (Infrequent Access)
1-5 years:   Glacier Instant Retrieval
5+ years:    Glacier Deep Archive
```

Cross-region replication to a secondary region for disaster recovery.

### Search indexes (Typesense)

Typesense data is **derived** from Postgres — not backed up directly. Recovery is via reindex from Postgres after restore.

For faster recovery, optional Typesense snapshots can be enabled (S3 export every 6 hours). But the canonical recovery path is reindex.

### Kafka

Kafka uses tiered storage to S3 (KRaft mode). Topics retained for 7 days hot, then archived. For hooks, this is sufficient — consumers are expected to be near real-time. For long-term event replay, the platform `event_log` table in Postgres is the source of truth.

### What Velocity itself backs up

CRDs are backed up via two mechanisms:

1. **GitOps repo** — all CRDs are applied via Git (ArgoCD or Flux). Git is the canonical store.
2. **Periodic etcd snapshots** — managed by the Kubernetes control plane (handled by cluster operators)

If the cluster is lost, restoring CRDs is `kubectl apply -f` from the Git repo.

---

## 2. Recovery Procedures

### Scenario 1 — Single schema corruption

A single table is corrupted (data quality issue, bad bulk update). Restore from PITR.

```bash
# Identify the timestamp before corruption
velocity history purchase-order --before-corruption

# Create a side-restore (does not affect production)
velocity restore schema purchase-order \
  --version v2 \
  --to '2026-05-16T09:00:00Z' \
  --target-schema acme_supply_chain_procurement_restore \
  --ticket JIRA-9001

# Operator provisions a new Postgres schema, restores from PITR,
# rebuilds time machine and search index for the restored data
# Original schema untouched

# Verify
velocity query purchase-order --schema-override restore --limit 10

# Promote (atomic swap)
velocity restore promote purchase-order --from-schema restore --ticket JIRA-9001
# Operator:
#   1. Quiesces traffic to purchase-order/v2 (returns 503 with Retry-After=10s)
#   2. RENAMEs schemas (atomic in Postgres)
#   3. Reindexes Typesense
#   4. Resumes traffic
# Total downtime: ~10s for this schema only
```

### Scenario 2 — Domain-level restore

Multiple schemas in a domain need rollback (bad migration, mass update).

```bash
velocity restore domain acme/supply-chain/procurement \
  --to '2026-05-16T09:00:00Z' \
  --ticket JIRA-9002

# Operator:
#   1. Creates {schema}_restore Postgres schema
#   2. Restores all tables under procurement domain from PITR
#   3. Applies current SchemaDefinitions to the restored data
#      (handles schema changes since the restore point — see §2.5)
#   4. Validates referential integrity
#   5. Awaits operator promotion
```

### Scenario 3 — Full Velocity restore from disaster

The Kubernetes cluster is destroyed. Bring up a new cluster.

```bash
# 1. New Kubernetes cluster ready, kubectl configured

# 2. Apply core CRD definitions
kubectl apply -f crds/

# 3. Apply Velocity operator and API
helm install velocity-operator charts/velocity-operator
helm install velocity-api      charts/velocity-api

# 4. Restore Postgres (CNPG handles this from S3 backup)
kubectl apply -f restore/velocity-postgres-restore.yaml
# Wait for CNPG to complete recovery from S3 (~30 min for typical size)

# 5. Apply organisational CRDs from Git
kubectl apply -f gitops/organisations/
kubectl apply -f gitops/applications/
kubectl apply -f gitops/domains/

# 6. Apply AuthStrategies (must be before SchemaDefinitions)
kubectl apply -f gitops/auth/

# 7. Apply SchemaDefinitions
kubectl apply -f gitops/schemas/

# Operator detects existing Postgres tables and skips provisioning (idempotent)
# Operator detects current resource state matches CRD spec — no DDL changes needed

# 8. Reindex Typesense from Postgres
velocity search reindex --all --parallel 4

# 9. Verify
velocity health
velocity audit verify --window 1h
```

Total recovery time for a moderate deployment (50 schemas): ~2 hours.

### Scenario 4 — Schema changed since restore point

Common case: someone added a field to `purchase-order` yesterday. Restore point is 3 days ago. The backup has `v2` of the table, current CRD defines `v3`.

```
Restore process:
  1. Restored table has v2 schema (no new field)
  2. Operator compares restored DDL vs current SchemaDefinition
  3. Applies safe migrations (ADD COLUMN with default if non-nullable)
  4. If unsafe migration would be required → flags for manual review, halts restore
```

The migration path is the same as a forward migration — Velocity's `DdlBuilder` knows how to evolve.

### Scenario 5 — Orphan table detection

A schema was deleted but the table remains (operator was down during deletion).

```bash
velocity drift check

# Output:
# Orphan tables found:
#   acme_merchandising_pricing.old_price_config_v1
#     last touched: 2026-04-20
#     no corresponding SchemaDefinition exists
#
# Run: velocity drift quarantine <table> to move to quarantine schema
#      velocity drift restore <table> to recreate the SchemaDefinition
```

Quarantine moves the orphan to `{org}_archive_quarantine` schema for manual review.

---

## 3. Failover Procedures

### Postgres primary failover

CNPG handles automatic failover. From the API server's perspective:

```
1. Primary becomes unavailable
2. PgBouncer detects (TCP RST or timeout)
3. CNPG promotes a replica (~10s)
4. PgBouncer reconfigures (~5s)
5. API server retries failed requests with exponential backoff
6. In-flight transactions: rolled back, client receives 503 with Retry-After: 5
```

Client-visible impact: ~15s of elevated error rate, then full recovery.

### Redis failover

If using Redis Sentinel or Redis Cluster:
```
1. Primary fails
2. Sentinel/Cluster promotes a replica (~5s)
3. API server's Redis client reconnects automatically
4. Per ADR-003, revocation checks deny by default → some legitimate requests fail
5. Operators receive alert (Redis unavailable)
```

If Redis is single-instance (dev/test only):
```
1. Configure failOpen: true in AuthStrategy for known-public endpoints
2. Other endpoints will fail closed until Redis recovers
```

### API server pod failure

Standard Kubernetes patterns:
- Liveness probe at `/healthz` (returns 200 if process responsive)
- Readiness probe at `/readyz` (returns 200 only if registry synced)
- Pod disruption budget: minAvailable: 50%
- Anti-affinity: pods spread across nodes

In-flight requests on a failing pod:
```
1. Pod receives SIGTERM
2. Readiness probe returns 503 immediately (removes from service endpoints)
3. Existing requests complete (30s grace period)
4. Pod exits
5. Total interruption: 0s for new requests (routed elsewhere), <30s for in-flight
```

### Operator pod failure

Leader election handles this:
```
1. Active operator pod fails
2. Lease expires (~15s)
3. Standby operator acquires lease
4. Resumes reconciliation
```

No data plane impact — only reconciliation is paused for 15s.

### Typesense node failure

In 3-node cluster:
```
1. Node fails
2. Remaining nodes continue serving reads
3. Replication restores when node returns
4. API server's Typesense client load-balances across remaining nodes
```

Search latency may increase briefly. No data loss (replication factor 2).

### Kafka broker failure

Standard Kafka HA:
```
3-broker cluster, replication factor 3:
  1 broker fails → no impact (other 2 serve)
  2 brokers fail → producers throttle (min.insync.replicas=2)
  3 brokers fail → producers fail; hooks queue locally for 5 min
```

---

## 4. Operator Upgrades

### Rolling upgrade procedure

```bash
# 1. Test the new operator version in staging
helm upgrade velocity-operator charts/velocity-operator --version 1.2.0 \
  --kube-context staging

# 2. Run integration tests
velocity test --context staging

# 3. Upgrade production
helm upgrade velocity-operator charts/velocity-operator --version 1.2.0 \
  --kube-context production

# Helm performs:
#   - Apply new operator deployment
#   - Old operator finishes current reconcile, releases leader lease
#   - New operator pod starts, acquires leader lease
#   - Old pod terminates
# Total operator unavailability: ~15s
# Data plane (API) unaffected
```

### CRD schema changes between operator versions

When operator versions change the CRD schema:

```
Operator v1.0 → v1.1 (additive change — new optional field):
  Direct upgrade. Existing CRDs continue to work.

Operator v1.1 → v2.0 (breaking change):
  1. Operator supports both CRD versions during the transition (apiVersion v1 and v2)
  2. Conversion webhook converts on-the-fly
  3. Migration tool batch-converts existing CRDs:
       velocity migrate crds --from v1 --to v2
  4. After migration, operator drops v1 support in next minor version
```

### Rollback procedure

```bash
# If new operator version misbehaves
helm rollback velocity-operator --kube-context production

# Operator returns to previous version, resumes normal operation
# Note: any CRDs created with v2-only features may not reconcile cleanly until re-applied
```

---

## 5. Webhook Resilience

The validating webhook is critical infrastructure. If it's down, all CRD applies fail — including the ones to fix the webhook itself.

### Configuration

```yaml
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: velocity-validating-webhook
webhooks:
  # Most CRDs — fail closed
  - name: schemadefinition.velocity.sh
    failurePolicy: Fail
    timeoutSeconds: 10
    rules:
      - operations: [CREATE, UPDATE, DELETE]
        apiGroups: [velocity.sh]
        apiVersions: [v1]
        resources: [schemadefinitions]
  
  # Bootstrap CRDs — fail open (so we can recover the cluster)
  - name: organisation.velocity.sh
    failurePolicy: Ignore
    timeoutSeconds: 5
    rules:
      - operations: [CREATE, UPDATE, DELETE]
        apiGroups: [velocity.sh]
        apiVersions: [v1]
        resources: [organisations, applications, domains]
```

### Webhook bypass for cluster admins

Emergency bypass annotation:

```yaml
metadata:
  annotations:
    velocity.sh/skip-validation: "true"
    velocity.sh/skip-validation-reason: "JIRA-9999 emergency recovery"
```

Honored only if:
1. The request comes from a user with `cluster-admin` ClusterRole
2. The annotation includes a reason
3. The annotation expires (must be reapplied each time, prevents accidental long-term bypass)

The bypass is audited at the highest priority.

### Webhook deployment

```yaml
spec:
  replicas: 3
  affinity:
    podAntiAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        - labelSelector:
            matchLabels: {app: velocity-webhook}
          topologyKey: kubernetes.io/hostname
  
  # Liveness and readiness essential
  livenessProbe:
    httpGet: {path: /healthz, port: 9443}
    periodSeconds: 5
  readinessProbe:
    httpGet: {path: /readyz, port: 9443}
    periodSeconds: 5
```

Three replicas, spread across nodes. The TLS certificate is provisioned by cert-manager and auto-rotated.

---

## 6. Secrets Rotation

### JWKS keys (your OIDC server's signing keys)

Already handled by your existing OIDC server — Velocity reads JWKS endpoint and caches with auto-refresh on key ID change.

Rotation procedure:
```
1. Generate new signing key in OIDC server
2. Publish new key to JWKS endpoint (now has old + new keys)
3. Start signing new tokens with new key
4. Wait for token TTL (1 hour)
5. Retire old key from JWKS endpoint
```

Velocity handles all of this transparently — no Velocity-side action needed.

### API keys

API keys are immutable. Rotation = create new key, switch the consumer, revoke the old key.

```bash
# 1. Create new key with same scope
velocity create api-key sap-sync-key-2026q3 \
  --copy-from sap-sync-key \
  --expires 2027-01-01 \
  --ticket JIRA-9100

# 2. Update consumer (SAP sync service) to use new key
kubectl edit secret sap-sync-credentials  # update with new key

# 3. Verify consumer is using new key (audit log shows new actor)
velocity audit --actor sap-sync-key-2026q3 --since 5m

# 4. Revoke old key
velocity revoke api-key sap-sync-key --ticket JIRA-9100
```

### OIDC client secret

For OIDC strategies, the client secret is in a Kubernetes Secret referenced by the AuthStrategy.

Rotation:
```
1. Generate new client secret in OIDC provider
2. Update Kubernetes Secret with new value
3. Operator detects Secret change, reloads AuthStrategy config
4. Old secret remains valid in OIDC provider for grace period (1 hour)
5. Retire old secret
```

### Database credentials

CNPG handles Postgres credential rotation automatically (via cert-based authentication where supported, or password rotation).

### Audit chain integrity verification

Periodic verification:
```bash
# Verify entire chain (nightly cron)
velocity audit verify --full

# Verify a window
velocity audit verify --from 2026-05-01 --to 2026-05-16

# On tamper detection, alert via Kafka topic
# velocity.alerts.audit.tamper
```

---

## 7. Monitoring and Alerts

### Critical alerts (page on-call)

```
- Postgres primary unavailable > 30s
- API server error rate > 5% for 5 min
- Operator leader lease unrenewed > 60s
- Audit chain verification failure
- Revoked actor making successful requests
- Webhook 5xx rate > 1% for 5 min
- Schema reconcile error rate > 10% for 5 min
- Search index lag > 30s for 5 min
```

### Warning alerts (notify, no page)

```
- Postgres replica lag > 10s
- Disk usage > 75%
- Redis memory usage > 75%
- Kafka consumer lag > 10000 messages
- API server p99 latency > 2× target SLO
- Time machine warm tier query latency > 30s
- Schema approaching record count limit
```

### Routine checks

```bash
# Run these from CI/CD daily
velocity health --strict           # exits non-zero if any component unhealthy
velocity drift check               # detects orphan tables, schema mismatches
velocity audit verify --window 24h # nightly chain verification
velocity quota report              # quota usage by app
```

---

## 8. Capacity Planning

### Per-environment sizing guidelines

**Development** (single-node)
```
1× Postgres (8GB RAM, 50GB SSD)
1× Redis    (1GB)
3× Kafka    (4GB each)
1× Typesense (4GB)
2× API server replicas
1× Operator (no HA)
```

**Production — small** (up to 10M records)
```
3× Postgres (CNPG HA, 32GB RAM, 500GB SSD)
3× Redis    (Sentinel, 8GB)
3× Kafka    (16GB each)
3× Typesense (16GB)
4× API server replicas (min)
2× Operator (HA)
3× Webhook
3× LogProcessor
```

**Production — large** (up to 500M records)
```
3× Postgres (CNPG HA, 128GB RAM, 5TB NVMe + cold tablespace)
3× Redis Cluster (32GB)
5× Kafka (64GB each, KRaft mode with tiered storage)
5× Typesense (32GB)
30× API server replicas (HPA target)
2× Operator (HA)
3× Webhook
6× LogProcessor
PgBouncer per domain
Read replicas: 2× per write replica
```

### Capacity warning thresholds

The operator emits warnings when:
- App approaches `maxSchemas` (90% full)
- Schema approaches `maxRecordsPerSchema` (90% full)
- Domain Postgres schema exceeds 100GB (consider partitioning)
- Single table exceeds 50M rows (consider partitioning)
- Time machine hot tier exceeds 100GB per schema (review retention)

---

## 9. Performance Tuning Reference

### When P99 latency rises

```
Symptom                         Likely cause                    Fix
─────────────────────────────────────────────────────────────────────────
Read latency 100ms → 500ms     Index missing                   velocity describe schema; add filterable: true
Read latency steady but high   Table too large                 Add partitioning to schema
Write latency rising           Connection pool saturated       Increase domain pool_size
Search latency rising          Typesense memory pressure       Add Typesense node
Hook latency rising            Kafka producer queue full       Increase Kafka throughput, check consumer lag
Audit write latency            Chain lock contention           Confirm audit volume; consider per-replica chains (ADR-005 revisit)
History query slow             Hot tier too large              Lower hot retention, more aggressive warm migration
```

### Tuning database connections

```
Per API replica:    20 connections (config)
PgBouncer pool size:150 total
                    Per-domain: weighted by domain RPS
Postgres max_conn:  200
                    Reserved for backups, admin: 30
                    Available for application: 170
```

### Tuning CDC workers

```
Batch size:         100 (default)
Flush interval:     50ms
Worker count:       1 per Tier-3 schema (default)
                    can scale via SELECT ... FOR UPDATE SKIP LOCKED
```

---

## 10. Runbooks

Each runbook is a separate file under `runbooks/`:

- `postgres-failover.md` — primary failure response
- `operator-crash-loop.md` — diagnose and recover stuck operator
- `webhook-down.md` — bypass procedure when validating webhook unavailable
- `typesense-rebuild.md` — full search index rebuild
- `audit-tamper-detected.md` — incident response for chain integrity failure
- `data-corruption.md` — single record / table / domain recovery
- `slo-breach.md` — diagnose ongoing SLO violation
- `quota-exceeded.md` — quota increase request and emergency procedure
- `secret-leak.md` — rotate compromised API keys / OIDC client secrets

Each runbook follows the same structure:
1. Symptoms (how you know this is happening)
2. Verification (how to confirm)
3. Immediate mitigation (stop the bleeding)
4. Recovery (return to normal)
5. Post-incident (review, prevent recurrence)
