---
title: Postgres Failover (CNPG)
description: Handle primary database failure and cluster switchover
---

This runbook covers how to respond when the Postgres primary becomes unavailable. Velocity uses CloudNative-PG (CNPG) for high-availability clustering.

## Detection

Failover is automatic in CNPG. Watch for alerts:

```
alert: PgPrimaryDown
PostgresClusterStatus: degraded
  Primary replicas: 0/1
  Standby replicas: 2/2
```

Or query directly:

```bash
kubectl get cluster velocity -o jsonpath='{.status.phaseTime}' -n velocity-system
# If phase is "unhealthy", primary is unreachable

kubectl describe cluster velocity -n velocity-system
# Look for Conditions: ready=False, archiveReady=False
```

Check CNPG logs:

```bash
kubectl logs -n velocity-system -l cnpg.io/cluster=velocity -c postgres --tail=50 | grep -i failover
```

## Automatic Recovery

CNPG performs automatic failover (Quorum-based):

1. **Detection (30 seconds):** Primary unresponsive
2. **Election (5-10 seconds):** Healthy standby elected as new primary
3. **Switchover (15-30 seconds):** Other standbys reconnect
4. **Bootstrap (5 minutes):** New primary rebuilds WAL streaming

Typical total downtime: **5-10 minutes**. No manual intervention required.

## Manual Intervention (If Needed)

### Check Cluster Status

```bash
kubectl get cluster velocity -n velocity-system
```

Output:

```
NAME       AGE     PHASE        READYPG   READINESS
velocity   45d     unhealthy    1/3       False
```

If stuck in `unhealthy`, proceed to manual switchover.

### Option 1: Restart Primary Pod (Preferred)

If the primary crashed but k8s didn't detect it:

```bash
kubectl delete pod velocity-1 -n velocity-system
# CNPG will:
# 1. Promote standby-2 or standby-3 to primary
# 2. Recreate deleted pod as standby
# 3. Sync WAL from new primary
```

Wait for recovery:

```bash
kubectl wait --for=condition=ready pod velocity-1 -n velocity-system --timeout=5m
```

### Option 2: Force Switchover

If the primary is hung but pod is running:

```bash
kubectl annotate cluster velocity \
  cnpg.io/switchoverImmediateFlag="true" \
  --overwrite -n velocity-system
```

CNPG initiates switchover immediately. New primary elected within 1 minute.

### Option 3: Rebuild Replica

If a standby is corrupted:

```bash
# Identify bad replica
kubectl get pod velocity-2 -n velocity-system -o wide
# STATUS: CrashLoopBackOff or Not Ready

# Delete it
kubectl delete pod velocity-2 -n velocity-system

# CNPG bootstraps from primary
kubectl wait --for=condition=ready pod velocity-2 -n velocity-system --timeout=10m
```

## Verify Recovery

After failover completes:

```bash
# 1. Check cluster status
kubectl get cluster velocity -n velocity-system
# Phase should be "healthy", ReadyPG 3/3

# 2. Check pod status
kubectl get pods -n velocity-system -l cnpg.io/cluster=velocity
# All pods Ready 1/1

# 3. Check primary elected
kubectl exec -it velocity-1 -n velocity-system -- pg_controldata | grep "Database cluster state"
# Should show "in production" for primary

# 4. Connect and verify
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin -c "SELECT version();"

# 5. Check WAL replication lag
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin -c \
  "SELECT slot_name, active, restart_lsn FROM pg_replication_slots;"
# lag_bytes should be small (< 1 MB)
```

## DNS / Service Endpoint Update

Velocity API automatically connects to the primary through `velocity-rw` service:

```bash
# Verify DNS resolves to new primary
nslookup velocity-rw.velocity-system.svc.cluster.local
# Should return IPs of primary pod
```

No manual DNS updates needed; CNPG manages the service endpoints.

## Data Validation After Failover

Once primary is back in service:

```bash
# 1. Check record counts match
kubectl exec velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "SELECT schema_name, COUNT(*) FROM information_schema.tables WHERE table_type='BASE TABLE' GROUP BY schema_name;"

# 2. Check audit log integrity
velocity audit verify \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001

# 3. Run health checks
velocity status
# All components should show Ready
```

## Preventing Future Failovers

### Increase Resource Limits

If primary crashed due to OOM:

```bash
kubectl edit deployment -n velocity-system velocity-postgres
# Increase memory request/limit
# spec.resources.requests.memory: 8Gi → 16Gi
# spec.resources.limits.memory: 8Gi → 16Gi
```

### Monitor Replication Lag

Create alert for lagging standby:

```yaml
alert: ReplicationLagHigh
expr: max(pg_replication_slot_latest_restart_lsn) - min(pg_replication_slot_latest_restart_lsn) > 100000000  # 100 MB
for: 2m
annotations:
  summary: "Replication lag > 100 MB (network bottleneck?)"
```

### Check Disk Space

```bash
kubectl exec velocity-1 -n velocity-system -- df -h /var/lib/postgresql/data
# If > 80%, WAL archival may be stalled
```

### Verify Backup Schedule

CNPG auto-backs up to S3. Check if backup jobs are running:

```bash
kubectl get job -n velocity-system -l cnpg.io/cluster=velocity
# Should see daily backup jobs
```

## Incident Report

Post-incident checklist:

- [ ] Record failover timestamp and duration
- [ ] Collect CNPG logs: `kubectl logs -l cnpg.io/cluster=velocity --since=2h`
- [ ] Check if any writes were lost (compare audit IDs before/after)
- [ ] Review Prometheus metrics for CPU/memory spikes
- [ ] File incident ticket with root cause
- [ ] Update runbook if steps were unclear

## Contacts

- **CNPG Team:** #database Slack
- **Operator Team:** #platform Slack
- **On-call:** /oncall in Slack

