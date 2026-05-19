---
title: Audit & Compliance
description: Append-only chain, hash integrity, and audit verification
---

The audit log is the source of truth for compliance. Every mutation is recorded in an immutable, hash-linked chain.

## How Audit Works

Every CREATE, UPDATE, DELETE, or RESTORE operation is written to `platform.audit_log` via a stored procedure:

```sql
CALL platform.audit_insert(
  schema_path := 'acme/supply-chain/procurement/purchase-order/v1',
  entity_id := 'PO-001',
  operation := 'UPDATE',
  actor := 'ravi.kumar',
  old_value := '{"status":"draft",...}',
  new_value := '{"status":"approved",...}',
  reason := 'Approved per stakeholder request'
);
```

The stored procedure:
1. Assigns incremental `event_id`
2. Computes hash: `SHA256(event_id || timestamp || actor || old_value || new_value || previous_hash)`
3. Writes the row
4. Returns the new hash to the caller

This creates an **hash-linked chain** where each event includes the hash of the previous event.

## Audit Log Schema

```sql
CREATE TABLE platform.audit_log (
  event_id BIGINT PRIMARY KEY,
  event_hash VARCHAR(64) NOT NULL,  -- SHA256 hex
  prev_hash VARCHAR(64),             -- Chain linkage
  timestamp TIMESTAMPTZ NOT NULL,
  schema_path TEXT NOT NULL,         -- acme/supply-chain/procurement/purchase-order/v1
  entity_id TEXT NOT NULL,           -- PO-001
  operation TEXT NOT NULL,           -- CREATE | UPDATE | DELETE | RESTORE
  actor TEXT NOT NULL,               -- ravi.kumar
  old_value JSONB,                   -- State before change
  new_value JSONB,                   -- State after change
  reason TEXT,                       -- Optional; required if schema demands it
  fail_mode TEXT,                    -- Auth fail mode (REDIS_UNAVAILABLE_DENIED, etc.)
  fail_open_allowed BOOLEAN,         -- Was failing open permitted?
  
  PRIMARY KEY (event_id),
  INDEX (timestamp),
  INDEX (schema_path, entity_id)
);
```

All columns are immutable (no UPDATEs allowed).

## Query Audit Log

### Via REST API

```bash
curl -G https://api.velocity.acme.com/api/platform/audit \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'schema=acme/supply-chain/procurement/purchase-order/v1' \
  --data-urlencode 'entity_id=PO-001' \
  --data-urlencode 'limit=50'
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
      "new_value": {"id":"PO-001","status":"draft",...},
      "reason": null
    },
    {
      "event_id": 2,
      "timestamp": "2026-05-19T14:35:00Z",
      "actor": "anita.sharma",
      "operation": "UPDATE",
      "old_value": {"status":"draft"},
      "new_value": {"status":"approved"},
      "reason": "Approved per stakeholder request"
    }
  ]
}
```

### Via CLI

```bash
velocity audit list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001
```

### Via SQL

```sql
SELECT * FROM platform.audit_log
WHERE schema_path = 'acme/supply-chain/procurement/purchase-order/v1'
  AND entity_id = 'PO-001'
ORDER BY event_id;
```

## Verify Audit Chain Integrity

The audit chain is hash-linked. Verify that no tampering has occurred:

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

If tampering is detected:

```
✗ Audit chain invalid (hash mismatch at event 15)
  Event 14 hash: abc123... (✓)
  Event 15 hash: def456... expected abc789...
  Events 15-42 are suspect
  Action required: Investigate source, contact security team
```

### How Verification Works

The CLI fetches all audit events and recomputes the hash chain:

```python
prev_hash = ""
for event in events:
  computed_hash = SHA256(
    str(event.event_id) +
    str(event.timestamp) +
    event.actor +
    json.dumps(event.old_value) +
    json.dumps(event.new_value) +
    prev_hash
  )
  
  if computed_hash != event.event_hash:
    print(f"Tampering detected at event {event.event_id}")
    exit(1)
  
  prev_hash = computed_hash

print("Chain valid")
```

Any change to any event's data (timestamp, actor, values) breaks the chain.

## Required Reason Field

Some schemas require a reason for mutations:

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
spec:
  audit:
    requireReason: true
    reasonPattern: ".{10,}"  # Min 10 chars
```

If you try to UPDATE without providing a reason:

```bash
curl -X PATCH https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"status":"approved"}'
```

Response:

```json
{"code": "AUDIT_REASON_REQUIRED", "message": "Reason is required for mutations on this schema"}
```

Include the reason:

```bash
curl -X PATCH https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1/PO-001 \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "status": "approved",
    "reason": "Approved per stakeholder request after review"
  }'
```

## Audit of Auth Decisions

Every request logs its authentication and authorization outcome:

```sql
SELECT actor, operation, outcome, fail_mode, timestamp
FROM platform.audit_log
WHERE fail_mode IS NOT NULL
ORDER BY timestamp DESC LIMIT 10;
```

Output:

```
actor      operation outcome                  fail_mode
────────────────────────────────────────────────────────
ravi.kumar CREATE    success                  NONE
anita.sharma READ    denied_rbac             NONE
bot@acme   UPDATE    denied_auth_invalid     NONE
─────────────────────────────────────────────────────
system     READ      success_revocation_denied REDIS_UNAVAILABLE_DENIED
```

This helps with:
- Debugging auth failures
- Detecting suspicious patterns (brute force attempts)
- Compliance audits ("Who tried to access this data?")

## Sensitive Field Redaction in Audit

Sensitive fields (marked with `sensitivity: pii`) are redacted in audit entries:

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

When you view audit log, the `credit_card` field shows as `****1234` (not plaintext).

```json
{
  "new_value": {
    "id": "PO-001",
    "credit_card": "****1234",  # Not the full card
    "status": "approved"
  }
}
```

## Long-Term Retention & Export

Audit log is partitioned by month for efficient archival:

```sql
CREATE TABLE platform.audit_log_2026_05 PARTITION OF platform.audit_log
  FOR VALUES FROM ('2026-05-01') TO ('2026-06-01');
```

Export to S3 for long-term storage (7 years):

```bash
# Automated by lifecycle job
pg_dump -t platform.audit_log_2026_04 velocity | gzip | \
  aws s3 cp - s3://velocity-audit-archive/2026-04/audit_log.sql.gz
```

Or query via warm-reader if needed for compliance.

## Prometheus Metrics

```
velocity_audit_events_total{operation="create", schema="..."} 15042
velocity_audit_events_total{operation="update", schema="..."} 3201
velocity_audit_chain_tampering_detected_total 0
velocity_audit_verification_duration_seconds 0.234
```

Alert on tampering:

```yaml
alert: AuditTamperingDetected
expr: increase(velocity_audit_chain_tampering_detected_total[5m]) > 0
for: 1m
severity: critical
annotations:
  summary: "Audit chain tampering detected in {{ $labels.schema }}"
  action: "Quarantine schema, contact security team immediately"
```

## Use Cases

### Compliance Audit Trail

Export audit log for external auditors:

```bash
velocity audit list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --output json > po_audit_2026_q1.json

# Or export to CSV for Excel
velocity audit list ... --output csv > po_audit_2026_q1.csv
```

Include in your compliance report.

### Dispute Resolution

Customer claims a value was different:

```bash
# Query audit for that entity
velocity audit list \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --entity-id PO-001

# Check the timestamps and old_value/new_value
# Provide to dispute resolution team
```

The hash chain proves you didn't modify history after the fact.

### Incident Investigation

An unauthorized update occurred. Find who and when:

```bash
# Query by actor and time range
SELECT * FROM platform.audit_log
WHERE actor = 'unknown-user'
  AND timestamp > now() - interval '7 days'
  AND operation IN ('UPDATE', 'DELETE');
```

Or use the CLI:

```bash
velocity audit list \
  --actor unknown-user \
  --since 7d \
  --operation UPDATE,DELETE
```

## Compliance Standards

Audit log design follows:

- **SOC 2 Type II:** Immutable audit trail, hash integrity
- **GDPR:** Reason field, actor identification, PII redaction
- **HIPAA:** Sensitivity tagging, field-level redaction, tamper detection
- **PCI-DSS:** Cardholder data redaction, access logging
- **SOX:** Separation of duties (different roles in approval chain)

## Best Practices

1. **Never disable audit:** It's always on, by design.
2. **Require reasons for sensitive changes:** Set `audit.requireReason: true` on high-risk schemas.
3. **Verify chain regularly:** Run `velocity audit verify` nightly as a health check.
4. **Export monthly:** Archive audit logs to S3 for long-term retention.
5. **Alert on tampering:** Set up Prometheus alert on `velocity_audit_chain_tampering_detected_total`.
6. **Review suspicious patterns:** Monitor for `fail_mode: REDIS_UNAVAILABLE_DENIED` (dependency failure).
7. **Redact sensitive fields:** Mark PII fields with `sensitivity: pii` and configure masking.

## Limitations

- Audit log is append-only; you cannot edit or delete audit entries (by design).
- Reason field is optional unless explicitly required by schema policy.
- Warm-tier audit queries (> 90 days) are slower (warm-reader).
