---
title: Time Machine
description: Point-in-time query, restore, and tiered storage
---

Time Machine records every change to every record and lets you query or restore any point in time. All schemas have time machine enabled by default.

## How It Works

When you create or update a record, two things happen in a single transaction:

1. The main table is updated
2. A history entry is written (INSERT into history table)

This is atomic: either both succeed or neither.

```sql
-- Before: main table has PO-001 with status=draft
UPDATE purchase_order_v1 SET status = 'approved' WHERE id = 'PO-001';

-- Atomically:
INSERT INTO purchase_order_v1_history 
  (entity_id, operation, old_value, new_value, actor, timestamp)
VALUES 
  ('PO-001', 'UPDATE', '{"status":"draft",...}', '{"status":"approved",...}', 'ravi.kumar', now());
```

Both statements complete together, or neither executes.

## Storage Tiers

### Hot Tier (0-90 days)

Data lives in Postgres `{table}_history` partitioned by `occurred_at`. Instant query, full fidelity.

```bash
# Query hot tier
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'at=2026-05-19T14:30:00Z'
```

Partition strategy: monthly. Partitions older than 90 days are automatically moved to S3 (warm tier).

### Warm Tier (90 days - 5 years)

Data lives in S3 Parquet format, queryable via the warm-reader (DataFusion). Slightly slower, but still interactive.

Queries transparently fall back to the warm tier if the hot query doesn't find data:

```bash
# Query warm tier (automatically)
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'at=2025-12-01T00:00:00Z'  # 5+ months ago
```

The API server queries the warm-reader RPC internally. To you, it's transparent.

### Cold Tier (5+ years)

Data moved to Glacier or Deep Archive. Restore required (deferred to Phase 10).

## Operations

### List History

Get all changes to a record:

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'limit=50' \
  --data-urlencode 'offset=0'
```

Response:

```json
{
  "data": [
    {
      "event_id": 1,
      "timestamp": "2026-05-19T14:32:00Z",
      "actor": "ravi.kumar",
      "operation": "CREATE",
      "old_value": null,
      "new_value": {"id":"PO-001","status":"draft","amount":50000}
    },
    {
      "event_id": 2,
      "timestamp": "2026-05-19T14:35:00Z",
      "actor": "anita.sharma",
      "operation": "UPDATE",
      "old_value": {"status":"draft"},
      "new_value": {"status":"approved"}
    }
  ],
  "pagination": { "limit": 50, "offset": 0, "total": 42 }
}
```

### Point-in-Time Query

Reconstruct the state of a record at a specific moment:

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'at=2026-05-19T14:33:00Z'
```

Response: the record's state at 2026-05-19 14:33:00 (between the CREATE and UPDATE):

```json
{
  "data": {
    "id": "PO-001",
    "status": "draft",
    "amount": 50000,
    "created_at": "2026-05-19T14:32:00Z",
    "version": 1
  }
}
```

### Diff

Compare record state between two timestamps:

```bash
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/diff \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'from=2026-05-19T14:32:00Z' \
  --data-urlencode 'to=2026-05-19T14:35:00Z'
```

Response:

```json
{
  "data": {
    "added": {
      "approval_date": "2026-05-19T14:35:00Z"
    },
    "changed": {
      "status": {"from":"draft","to":"approved"}
    },
    "removed": {}
  }
}
```

### Restore

Write a new event that restores a record to a past state:

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/restore \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "at": "2026-05-19T14:33:00Z",
    "reason": "Approval was incorrect; customer dispute. Reverting to draft per stakeholder request."
  }'
```

Response (201 Created):

```json
{
  "data": {
    "id": "PO-001",
    "status": "draft",
    "version": 3,
    "restored_at": "2026-05-19T14:40:00Z"
  }
}
```

Note: Restore writes a **new event**. It does not delete the UPDATE event; it creates a RESTORE event. The full history is preserved.

History after restore:

```
Event 1: CREATE (draft)
Event 2: UPDATE (approved)
Event 3: RESTORE (back to draft, reason: "Approval was incorrect...")
```

### Replay

Stream all events for a record (Server-Sent Events):

```bash
curl -N https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/replay \
  -H "Authorization: Bearer $TOKEN"
```

Response (text/event-stream):

```
data: {"event_id":1,"timestamp":"2026-05-19T14:32:00Z","operation":"CREATE",...}
data: {"event_id":2,"timestamp":"2026-05-19T14:35:00Z","operation":"UPDATE",...}
data: {"event_id":3,"timestamp":"2026-05-19T14:40:00Z","operation":"RESTORE",...}
```

### Snapshots

Take a point-in-time snapshot of **all records** in a schema:

```bash
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/history/snapshot \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "at": "2026-05-01T00:00:00Z",
    "reason": "Monthly snapshot for compliance archive"
  }'
```

Response (202 Accepted):

```json
{
  "data": {
    "snapshot_id": "snap-abc123xyz",
    "schema": "acme/supply-chain/procurement/purchase-order/v1",
    "timestamp": "2026-05-01T00:00:00Z",
    "status": "in_progress",
    "expires_at": "2026-06-01T00:00:00Z"
  }
}
```

Snapshots are exported to S3 and are queryable for compliance audits.

## Configuration

Customize time-machine retention per schema:

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: purchase-order
spec:
  timeMachine:
    enabled: true
    hotRetention: 90d      # How long to keep in Postgres
    warmRetention: 5y      # How long to keep in S3
    coldRetention: none    # Glacier (deferred)
    snapshotRetention: 7d  # Keep snapshots for 7 days
```

### Retention Policies

- **Hot (0-90 days):** Partitioned Postgres table, instant query
- **Warm (90d-5y):** S3 Parquet, queryable via warm-reader (DataFusion)
- **Cold (5y+):** Glacier Deep Archive, restore required (Phase 10)

After 90 days, the operator automatically exports the Postgres history partition to S3 and drops the Postgres partition. The warm-reader takes over.

## Redaction in History

Sensitive fields are redacted in history entries per the schema's field configuration:

```yaml
spec:
  fields:
    - name: credit_card
      type: string
      sensitivity: pii
      masking:
        strategy: partial
        visibleChars: 4
```

When you query history, the `credit_card` field in old_value/new_value shows as `****1234` (not plaintext).

## Audit of Time-Machine Operations

Every restore is recorded in the audit log:

```sql
SELECT actor, operation, entity_id, reason FROM platform.audit_log
WHERE operation = 'RESTORE'
ORDER BY timestamp DESC;
```

Output:

```
actor         operation entity_id  reason
─────────────────────────────────────────────────────────────────
ravi.kumar    RESTORE   PO-001     Approval was incorrect...
```

## Use Cases

### Accidental Data Deletion Recovery

User accidentally soft-deletes a record:

```bash
# Check when it was good
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN"

# Restore it
curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/restore \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"at":"2026-05-19T14:30:00Z","reason":"Accidental delete; customer follow-up required"}'
```

### Dispute Investigation

Customer claims a value was different on a date:

```bash
# Check history
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/history \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'at=2026-04-19T00:00:00Z'

# Compare with now
curl -G https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/diff \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'from=2026-04-19T00:00:00Z' \
  --data-urlencode 'to=2026-05-19T00:00:00Z'
```

Provide the diff and audit trail to dispute resolution team.

### Compliance Snapshots

Take monthly snapshots for regulatory audit:

```bash
for month in {01..12}; do
  curl -X POST https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/history/snapshot \
    -H "Authorization: Bearer $TOKEN" \
    -d "{\"at\":\"2026-${month}-01T00:00:00Z\",\"reason\":\"Monthly compliance snapshot\"}"
done
```

Snapshots are immutable and exportable for audit.

### Operational Diagnostics

Trace how a state was reached:

```bash
# Full replay of PO-001
curl -N https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001/replay \
  -H "Authorization: Bearer $TOKEN" | jq '.'
```

Shows exact sequence of who did what when. Invaluable for debugging production incidents.

## Limitations & Deferred Features

### Deferred

- **Cold-tier restore (Glacier):** Phase 10. For now, data in Glacier is immutable.
- **Selective restore (restore some fields only):** Phase 10. Restore is all-or-nothing.
- **Warm-tier cross-dataset joins:** Phase 4 revision. Warm-reader is single-table only.

### Current Behavior

- History is write-once, read-many. You cannot edit history (immutable by design).
- Restore is a new event, not a rollback. All previous events remain visible.
- RLS applies to history queries. You can only see history for records you can read now.

## Performance Notes

- **Hot query (< 90 days):** < 10ms (Postgres index scan)
- **Warm query (90d-5y):** 100-500ms (S3 + DataFusion scan)
- **Large snapshots (> 1M records):** Background job, 202 Accepted response

For frequently-accessed warm-tier data, consider archiving selectively (don't archive high-frequency records).
