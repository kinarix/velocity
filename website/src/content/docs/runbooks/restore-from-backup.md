---
title: Restore from Backup
description: Recover Postgres data from S3 backup
---

Use this runbook when you need to restore Postgres to a point in time from an S3 backup.

## Scenarios

- **Data corruption:** Undetected DDL error or buggy data migration
- **Accidental deletion:** Someone truncated a table
- **Ransomware:** Attacker modified records; restore to clean point
- **Test data spillage:** Prod data was accidentally overwritten with test data

## Prerequisites

- CNPG cluster healthy (if only restoring one table)
- S3 backup available and accessible
- `pgBackRest` client installed locally
- Access to Kubernetes cluster

## Step 1: Identify Backup Point

List available backups:

```bash
# List backups in S3
aws s3 ls s3://velocity-backups/postgres/ --recursive | grep backup.info
# Output:
# 2026-05-19 14:32 backup-2026-05-19_14-32-56.tar
# 2026-05-18 02:15 backup-2026-05-18_02-15-30.tar
```

Or use CNPG's backup listing:

```bash
kubectl get backups -n velocity-system -o wide
```

Output:

```
NAME                                 BACKUP PHASE   REFERENCE NAME
velocity-20260519-143256             succeeded      velocity
velocity-20260518-021530             succeeded      velocity
```

Determine target restore point:

```bash
# Restore to latest backup
BACKUP_ID=velocity-20260519-143256

# Or restore to specific timestamp
# (must be within WAL archival window — typically 30 days)
RESTORE_TIME="2026-05-19 10:00:00"
```

## Step 2: Create Restore Cluster (Preferred Method)

Create a temporary cluster to restore into, then validate:

```bash
cat > /tmp/restore-cluster.yaml <<EOF
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: velocity-restore
  namespace: velocity-system
spec:
  instances: 1
  bootstrap:
    recovery:
      source: velocity
      recoveryTarget:
        name: $BACKUP_ID
        # OR for time-based restore:
        # timeline: latest
        # inclusive: false
        # backupID: $BACKUP_ID
        # targetTime: "$RESTORE_TIME"
  postgresql:
    parameters:
      shared_buffers: 4GB
      max_connections: 100
  storage:
    size: 500Gi
    storageClass: ssd
  externalClusters:
    - name: velocity
      connectionParameters:
        host: velocity-rw.velocity-system.svc.cluster.local
        port: "5432"
        user: postgres
      barmanObjectStore:
        destinationPath: s3://velocity-backups/postgres
        s3Credentials:
          accessKeyId:
            name: aws-s3-creds
            key: ACCESS_KEY_ID
          secretAccessKey:
            name: aws-s3-creds
            key: SECRET_ACCESS_KEY
EOF

kubectl apply -f /tmp/restore-cluster.yaml
```

Wait for restoration:

```bash
kubectl wait --for=condition=ready cluster velocity-restore -n velocity-system --timeout=30m
```

Verify data:

```bash
kubectl exec -it velocity-restore-1 -n velocity-system -- psql -U postgres -c \
  "SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1;"
```

## Step 3: Validate Restored Data

Run sanity checks:

```bash
# Check audit chain integrity
kubectl exec -it velocity-restore-1 -n velocity-system -- psql -U postgres -c \
  "SELECT COUNT(*), COUNT(DISTINCT event_hash) FROM platform.audit_log LIMIT 1000;"
# row_count should equal distinct event_hash count (no duplicates)

# Check for data anomalies
kubectl exec -it velocity-restore-1 -n velocity-system -- psql -U postgres -c \
  "SELECT COUNT(*) as deleted_records FROM acme_supply_chain_procurement.purchase_order_v1 WHERE deleted_at IS NOT NULL;"

# Compare record counts with production
kubectl exec -it velocity-1 -n velocity-system -- psql -U postgres -c \
  "SELECT COUNT(*) as prod_count FROM acme_supply_chain_procurement.purchase_order_v1;"

kubectl exec -it velocity-restore-1 -n velocity-system -- psql -U postgres -c \
  "SELECT COUNT(*) as restore_count FROM acme_supply_chain_procurement.purchase_order_v1;"
# If counts match, restoration is good
```

## Step 4: Swap Production (If Validation Passes)

Once validated, swap the production cluster:

```bash
# 1. Scale down API to 0 (prevent writes)
kubectl scale deployment velocity-api -n velocity-system --replicas=0

# 2. Rename clusters
kubectl patch cluster velocity -n velocity-system -p '{"metadata":{"name":"velocity-old"}}'
kubectl patch cluster velocity-restore -n velocity-system -p '{"metadata":{"name":"velocity"}}'

# 3. Restart API
kubectl scale deployment velocity-api -n velocity-system --replicas=3

# 4. Verify API health
kubectl wait --for=condition=ready deployment velocity-api -n velocity-system --timeout=5m
velocity status
```

## Step 5: Cleanup Old Cluster

Once confirmed, delete the old cluster:

```bash
# Keep the old cluster for 24 hours in case rollback needed
kubectl delete cluster velocity-old -n velocity-system

# If everything is stable, delete the PVC too
kubectl delete pvc velocity-old-1 -n velocity-system
```

## Alternative: Restore Single Table

If only one table is corrupted:

```bash
# 1. Export table from backup (without modifying production)
pg_dump -h velocity-restore-1.velocity-system.svc.cluster.local \
  -U postgres \
  -t acme_supply_chain_procurement.purchase_order_v1 \
  -F c velocity > /tmp/purchase_order_backup.dump

# 2. Truncate corrupted table in production
kubectl exec -it velocity-1 -n velocity-system -- psql -U postgres -c \
  "TRUNCATE acme_supply_chain_procurement.purchase_order_v1;"

# 3. Restore from dump
pg_restore -h velocity-rw.velocity-system.svc.cluster.local \
  -U postgres \
  -d velocity \
  -t acme_supply_chain_procurement.purchase_order_v1 \
  /tmp/purchase_order_backup.dump

# 4. Verify integrity
velocity audit verify --schema acme/supply-chain/procurement/purchase-order/v1
```

## Data Loss Assessment

Determine what data was lost:

```bash
# Query audit log for when corruption occurred
kubectl exec velocity-1 -n velocity-system -- psql -U postgres -c \
  "SELECT event_id, timestamp, actor, operation, entity_id
   FROM platform.audit_log
   WHERE schema_path = 'acme/supply-chain/procurement/purchase-order/v1'
   AND timestamp > '2026-05-19 00:00:00'
   ORDER BY event_id DESC LIMIT 20;"

# Compare with restored data
kubectl exec velocity-restore-1 -n velocity-system -- psql -U postgres -c \
  "SELECT COUNT(*), MAX(created_at) FROM acme_supply_chain_procurement.purchase_order_v1;"
```

## Post-Restoration Checklist

- [ ] Verify API is responding (curl /healthz)
- [ ] Confirm record counts match expected
- [ ] Run `velocity audit verify` on affected schemas
- [ ] Notify affected users of restoration
- [ ] Review backup strategy (how did this happen?)
- [ ] File incident ticket with timeline
- [ ] Update on-call playbook if steps were unclear

## Contacts

- **Database Team:** #database Slack
- **Incident Commander:** /page-oncall in Slack

