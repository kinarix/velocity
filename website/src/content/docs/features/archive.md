---
title: Archive & Purge
description: Automatic tiering, S3 Parquet export, and lifecycle management
---

Archive moves old records from hot Postgres to cold S3 Parquet storage. Purge permanently deletes records after they've aged out.

## How It Works

The archive worker periodically scans schemas for records matching an ArchivePolicy and exports them to S3 in Parquet format.

```
Hot (Postgres) ──archive──> Warm (S3 Parquet) ──purge──> Deleted
  0-90 days                90d-5y                 5+ years
  RLS enforced            queryable via warm-reader    gone
```

Once archived, a record is no longer in the hot table but is still queryable via `/archive/query`.

## ArchivePolicy CRD

```yaml
apiVersion: velocity.sh/v1
kind: ArchivePolicy
metadata:
  name: po-archive
  namespace: acme-supply-chain-procurement
spec:
  schemaPath: acme/supply-chain/procurement/purchase-order/v1
  
  trigger:
    type: age  # age | size | cel (cel deferred)
    days: 90   # Archive after 90 days
  
  destination:
    type: s3
    bucket: velocity-archives
    region: us-east-1
    prefix: acme/supply-chain/procurement
    storageClass: STANDARD_IA  # Cost optimization
  
  format: parquet
  
  batchSize: 10000  # Records per batch
  
  purgePolicy:
    enabled: true
    afterDays: 2555  # Delete after 7 years
    requiresApproval: true
  
  schedule: "0 2 * * *"  # Daily at 2 AM UTC
```

Create it:

```bash
kubectl apply -f archive-policy.yaml
```

## Archive Operations

### Archive a Record

The operator automatically archives records matching the policy. Manual archive (Phase 9) coming soon.

### Query Archived Records

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/archive/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "where": {
      "and": [
        { "field": "status", "op": "eq", "value": "delivered" },
        { "field": "created_at", "op": "lt", "value": "2026-02-01T00:00:00Z" }
      ]
    },
    "select": ["id", "supplier_code", "amount"],
    "limit": 100
  }'
```

Response (from warm-reader DataFusion scan of S3 Parquet):

```json
{
  "data": [
    {"id":"PO-00000001","supplier_code":"TATA001","amount":50000},
    {"id":"PO-00000002","supplier_code":"ACC001","amount":60000}
  ],
  "stats": {
    "files_scanned": 3,
    "rows_examined": 125000,
    "elapsed_ms": 234
  }
}
```

### Get Archived Record by ID

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/archive \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'version=42'  # Optional; gets specific version
```

### Unarchive (Restore to Hot)

Move a record back to hot Postgres storage:

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/unarchive \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"reason": "Customer dispute; needs investigation"}'
```

Response:

```json
{
  "data": {
    "id": "PO-001",
    "status": "delivered",
    "unarchived_at": "2026-05-19T14:40:00Z"
  }
}
```

The record is now back in the hot table and queryable via normal endpoints.

## Trigger Types

### Age-Based (Implemented)

Archive records older than N days:

```yaml
trigger:
  type: age
  days: 90
```

Runs daily. Finds records with `created_at < now() - 90 days` and `status != deleted`.

### Size-Based (Implemented)

Archive when table reaches N GB:

```yaml
trigger:
  type: size
  gigabytes: 50
```

When `pg_size_pretty(pg_total_relation_size(table)) > 50GB`, archive the oldest records until table shrinks below 50GB.

### CEL (Deferred)

Custom expression:

```yaml
trigger:
  type: cel
  condition: "self.status in ['archived', 'cancelled'] && self.updated_at < now() - duration('7776000s')"
```

Deferred to Phase 10. Not yet implemented.

## PurgeRequest Workflow

To permanently delete archived records, submit a PurgeRequest:

```yaml
apiVersion: velocity.sh/v1
kind: PurgeRequest
metadata:
  name: q1-2024-purge
  namespace: acme-supply-chain-procurement
spec:
  archivePolicy: po-archive
  
  criteria:
    createdBefore: "2024-03-31T23:59:59Z"
    status: [delivered, archived]
  
  reason: "Q1 2024 records aged out per retention policy"
  
  estimatedRecords: 125000
  
  requiresApproval: true
```

Create it:

```bash
kubectl apply -f purge-request.yaml
```

The request is held in `Pending` state until approved (Phase 8).

### Approve a PurgeRequest

```bash
velocity approve \
  --purge-request q1-2024-purge \
  --reason "Approved by compliance team"
```

Or via the CRD:

```bash
kubectl annotate purgeRequest q1-2024-purge \
  velocity.sh/approved-by=ravi.kumar \
  velocity.sh/approved-at=$(date -Iseconds) \
  --overwrite
```

Once approved, the archive worker deletes the records.

## Archive File Format

Records are exported as **Parquet** (columnar format):

```
s3://velocity-archives/acme/supply-chain/procurement/
├── 2026-05/
│   ├── purchase_order_v1_2026-05-01_batch-001.parquet
│   ├── purchase_order_v1_2026-05-01_batch-002.parquet
│   └── ...
└── 2026-04/
    ├── purchase_order_v1_2026-04-01_batch-001.parquet
    └── ...
```

**Advantages of Parquet:**
- Compressed (40-60% savings vs JSON)
- Columnar (fast filtering on specific fields)
- Schema inferred from SchemaDefinition
- Query via warm-reader (DataFusion SQL)

**Compatibility:**
- Readable by Spark, Presto, BigQuery, Athena
- Compatible with modern data warehouses for analytics

## Retention Tiers

### Postgres Hot (0-90 days)

- Storage: SSD (fast)
- Queryable: Full RLS enforcement, all endpoints
- Cost: High

### S3 Warm (90d-5y)

- Storage: S3 Standard-IA (cost-optimized)
- Queryable: `/archive/query` endpoint, via warm-reader
- Cost: Low ($0.02/GB)
- RLS: Applied at query time

### Glacier Cold (5y+)

- Storage: Glacier Deep Archive
- Queryable: Restore required (Phase 10)
- Cost: Very low ($0.004/GB)

Configure per schema:

```yaml
spec:
  timeMachine:
    hotRetention: 90d
    warmRetention: 5y
  
  archive:
    purgePolicy:
      afterDays: 1825  # 5 years
      storageClass: GLACIER_DEEP_ARCHIVE
```

## Monitoring Archive Health

### Check Archive Runs

```sql
SELECT schema_path, status, started_at, completed_at, records_archived 
FROM platform.archive_runs 
ORDER BY started_at DESC LIMIT 10;
```

Output:

```
schema_path                                        status    records_archived
─────────────────────────────────────────────────────────────────────────────
acme/supply-chain/procurement/purchase-order/v1   success   25000
acme/supply-chain/procurement/purchase-order/v1   success   30000
```

### Monitor Archive Worker Logs

```bash
kubectl logs -n velocity-system -l app=velocity-archive-worker --tail=100 | jq '.'
```

Look for:
- Successful batches: `"status":"archived_batch","records":10000`
- Errors: `"level":"error","schema":"..."` (investigate)
- Performance: `"elapsed_ms":5420` (slow scans?)

### Prometheus Metrics

```
velocity_archive_runs_total{schema="...", status="success"} 150
velocity_archive_records_total{schema="...", destination="s3"} 5000000
velocity_archive_duration_seconds{schema="..."} 180.5
```

Alert on stalled archives:

```yaml
alert: ArchiveStalled
expr: time() - max(platform.archive_runs.completed_at by (schema)) > 86400
for: 1h
annotations:
  summary: "Archive for {{ $labels.schema }} has not run in 24 hours"
```

## Cost Estimation

Assuming 100K records/month at avg 10 KB each:

- **Hot (Postgres):** 900 GB/year, ~$500/year (SSD)
- **Warm (S3 Standard-IA):** 5 TB/5 years, ~$100/year ($0.02/GB)
- **Purge cold:** No cost after deletion

Total: ~$60-100/year for full time-machine coverage on 100K records/month.

## Best Practices

1. **Archive aggressively:** 90 days is a good default. Adjust per data volume.
2. **Require approval for purge:** Don't auto-delete; compliance teams need audit trail.
3. **Test unarchive:** Periodically restore archived records to ensure warm-reader works.
4. **Monitor warm-reader latency:** If archive/query is > 500ms, optimize batch size or S3 prefix structure.
5. **Use predictable naming:** Keep S3 prefixes consistent so warm-reader scans efficiently.
6. **Lifecycle rules:** Enable S3 lifecycle rules to transition to Glacier after 1 year:
   ```json
   {
     "Rules": [{
       "Prefix": "acme/supply-chain/",
       "Status": "Enabled",
       "Transitions": [{
         "Days": 365,
         "StorageClass": "GLACIER_IR"
       }]
     }]
   }
   ```

## Deferred Features

- **CEL trigger:** Custom expressions for archive eligibility (Phase 10)
- **Typed Arrow columns:** Preserve schema type info in Parquet (Phase 10)
- **Orphan sweep:** Delete archived records whose source records were purged (Phase 10)
- **Sharded archive workers:** Multiple workers for parallelism (Phase 11)
