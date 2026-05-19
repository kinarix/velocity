---
title: Quarantine Drifted Schema
description: Handle schema state mismatch between CRD and Postgres
---

Schema drift occurs when the Postgres table state doesn't match the CRD spec. This runbook walks you through detection, understanding the drift, and recovery.

## Detection

Drift is detected by the operator during reconciliation. Watch for warnings:

```bash
kubectl describe schemadefiniton purchase-order -n acme-supply-chain-procurement
# Look for Conditions: Reconcile=False, Reason="SchemaDrift"
```

Or check operator logs:

```bash
kubectl logs -n velocity-system -l app=velocity-operator --tail=100 | grep -i drift
# Output:
# schema purchase-order has drifted: index idx_po_supplier missing in Postgres
```

Check API error responses:

```bash
curl https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN"

# Output:
# {"code": "SCHEMA_DRIFT", "message": "Postgres table state does not match CRD"}
```

## Understand the Drift

Query the Postgres table directly to see what's different:

```bash
# Connect to Postgres
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity

# Check table structure
\d acme_supply_chain_procurement.purchase_order_v1
```

Output example:

```
Table "acme_supply_chain_procurement.purchase_order_v1"
 Column     |           Type            | Collation | Nullable | Default
────────────┼───────────────────────────┼───────────┼──────────┼─────────
 id         | text                      |           | not null |
 supplier_code | text                   |           | not null |
 amount     | numeric                   |           | not null |
 status     | text                      |           | not null |
 created_at | timestamp with time zone  |           | not null | now()
 updated_at | timestamp with time zone  |           |          |
 deleted_at | timestamp with time zone  |           |          |

Indexes:
    "purchase_order_v1_pkey" PRIMARY KEY, btree (id)
    "idx_po_supplier" btree (supplier_code)  ← This may be missing
    "idx_po_created_at" btree (created_at)   ← Or this
```

Check for expected indexes, constraints, and RLS policies:

```sql
-- List all indexes on the table
SELECT indexname FROM pg_indexes 
WHERE schemaname = 'acme_supply_chain_procurement' 
AND tablename = 'purchase_order_v1';

-- List all constraints
SELECT constraint_name, constraint_type 
FROM information_schema.table_constraints 
WHERE table_schema = 'acme_supply_chain_procurement' 
AND table_name = 'purchase_order_v1';

-- List RLS policies
SELECT policyname, permissive FROM pg_policies 
WHERE schemaname = 'acme_supply_chain_procurement' 
AND tablename = 'purchase_order_v1';
```

Compare with CRD definition:

```bash
kubectl get sd purchase-order -n acme-supply-chain-procurement -o yaml | grep -A 50 "fields:"
```

## Common Causes

### Missing Index

Someone ran `DROP INDEX idx_po_supplier` directly in Postgres.

**Recovery:**

```bash
# Step 1: Understand what the CRD expects
kubectl get sd purchase-order -n acme-supply-chain-procurement -o yaml | grep -A 5 "indexes:"

# Step 2: Recreate the index manually (or let operator do it)
# Manual:
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "CREATE INDEX CONCURRENTLY idx_po_supplier ON acme_supply_chain_procurement.purchase_order_v1(supplier_code);"

# OR let operator re-reconcile:
kubectl delete deployment velocity-operator -n velocity-system --wait=false
kubectl rollout restart deployment velocity-operator -n velocity-system
# Operator will re-apply DDL
```

### Missing Column

A required field was removed from the table.

**Recovery (if data exists):**

```bash
# Restore the column
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "ALTER TABLE acme_supply_chain_procurement.purchase_order_v1 ADD COLUMN approval_date timestamp;"
```

**Recovery (if data lost permanently):**

```bash
# This is unrecoverable without a backup. Restore from backup or accept data loss.
# Then reconcile CRD to match Postgres reality (breaking change annotation):
kubectl patch sd purchase-order -n acme-supply-chain-procurement -p \
  '{"metadata":{"annotations":{"velocity.sh/breaking-change":"approved"}}}'
```

### Constraint Violation

A unique or check constraint is missing.

**Recovery:**

```bash
# Check if adding the constraint would fail
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "ALTER TABLE acme_supply_chain_procurement.purchase_order_v1 ADD CONSTRAINT chk_amount_positive CHECK(amount > 0) NOT VALID;"
# Use NOT VALID to skip checking existing rows

# Then validate:
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "ALTER TABLE acme_supply_chain_procurement.purchase_order_v1 VALIDATE CONSTRAINT chk_amount_positive;"
```

### RLS Policy Missing

RLS policies were deleted or misconfigured.

**Recovery:**

```bash
# Check what policies should exist
kubectl get sd purchase-order -n acme-supply-chain-procurement -o yaml | grep -A 10 "rowFilter:"

# Re-create policies
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "CREATE POLICY region_isolation ON acme_supply_chain_procurement.purchase_order_v1 FOR ALL USING (region = current_setting('app.current_region'));"

# Enable RLS if disabled
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "ALTER TABLE acme_supply_chain_procurement.purchase_order_v1 ENABLE ROW LEVEL SECURITY;"
```

## Quarantine & Recovery

### Option 1: Auto-Fix (Safe for non-breaking drifts)

Let the operator re-reconcile and recreate missing indexes/constraints:

```bash
# 1. Restart operator to clear any stuck state
kubectl rollout restart deployment velocity-operator -n velocity-system

# 2. Force operator to reconcile
kubectl annotate sd purchase-order \
  -n acme-supply-chain-procurement \
  velocity.sh/force-reconcile="$(date)" \
  --overwrite

# 3. Wait for reconciliation
kubectl wait --for=condition=Reconcile=True sd purchase-order -n acme-supply-chain-procurement --timeout=5m

# 4. Verify API is healthy
curl https://api.velocity.acme.com/healthz
```

### Option 2: Manual Schema Rebuild (For Breaking Changes)

If the drift involves breaking changes (column removal, type change), manually reconcile:

```bash
# 1. Understand desired state from CRD
kubectl get sd purchase-order -n acme-supply-chain-procurement -o yaml > /tmp/desired.yaml

# 2. Understand current state in Postgres
kubectl exec -it velocity-1 -n velocity-system -- pg_dump -s velocity | grep -A 50 purchase_order_v1 > /tmp/actual.sql

# 3. Decide: Update CRD to match Postgres, or update Postgres to match CRD

# 4. If updating Postgres, apply DDL manually
# (Only if you understand the implications!)
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity < /tmp/fix.sql

# 5. Mark as approved (if breaking):
kubectl patch sd purchase-order -n acme-supply-chain-procurement -p \
  '{"metadata":{"annotations":{"velocity.sh/breaking-change":"approved"}}}'

# 6. Force reconcile
kubectl annotate sd purchase-order \
  -n acme-supply-chain-procurement \
  velocity.sh/force-reconcile="$(date)" \
  --overwrite
```

### Option 3: Recreate Schema (Nuclear Option)

If drift is severe and data can be re-imported:

```bash
# 1. Export data
kubectl exec velocity-1 -n velocity-system -- pg_dump -a velocity \
  --table=acme_supply_chain_procurement.purchase_order_v1 > /tmp/data.sql

# 2. Delete the CRD (marks table for deletion)
kubectl delete sd purchase-order -n acme-supply-chain-procurement

# 3. Wait for table to be dropped by operator
kubectl wait pod -n velocity-system -l app=velocity-operator --for condition=Ready --timeout=5m

# 4. Recreate the CRD from Git
kubectl apply -f purchase-order-schema.yaml

# 5. Wait for table to be created
kubectl wait --for=condition=Reconcile=True sd purchase-order -n acme-supply-chain-procurement --timeout=5m

# 6. Re-import data
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity < /tmp/data.sql

# 7. Verify
curl https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN"
# Should return 200 OK
```

## Validation

Once recovered, validate:

```bash
# 1. Check schema status
kubectl get sd purchase-order -n acme-supply-chain-procurement
# Status: Ready, no warnings

# 2. Describe to see no drift conditions
kubectl describe sd purchase-order -n acme-supply-chain-procurement
# Conditions should all be True

# 3. Query API to confirm working
curl https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1 \
  -H "Authorization: Bearer $TOKEN" \
  --data-urlencode 'limit=10'
# Should return 200 OK with records

# 4. Check audit chain integrity
velocity audit verify \
  --schema acme/supply-chain/procurement/purchase-order/v1
# Should show ✓ valid chain

# 5. Ensure RLS still works
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "SET app.current_region = 'west'; SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1;"
# Then:
kubectl exec -it velocity-1 -n velocity-system -- psql -U velocity_admin velocity -c \
  "SET app.current_region = 'east'; SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1;"
# Should return different counts if RLS is working
```

## Post-Incident

- [ ] Document root cause (how did drift happen?)
- [ ] Review operator RBAC (does it have sufficient permissions?)
- [ ] Check for stray kubectl apply or manual DDL commands
- [ ] Implement drift detection alert (if not already present)
- [ ] Brief team on schema management policy

## Contacts

- **Operator Team:** #platform Slack
- **Database Team:** #database Slack
- **On-call:** /page-oncall

