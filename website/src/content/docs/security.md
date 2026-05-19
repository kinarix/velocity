---
title: Security Architecture
description: Threat model, RLS enforcement, auth fail modes, and the last line of defence
---

Velocity's security model is built on three core principles: defense in depth, fail-closed by default, and cryptographic proof of integrity.

## Threat Model

### Attacker Profiles

1. **Compromised API server** (app code or dependency exploited)
   - Mitigation: Row-level security enforced by Postgres, not app
   - Status: Protected by ADR-007 (non-superuser role)

2. **Malicious or buggy authorization code in the app**
   - Mitigation: `SET LOCAL ROLE` per request; RLS cannot be bypassed
   - Status: Protected by ADR-007

3. **SQL injection from user input**
   - Mitigation: All queries use parameterized statements; no string interpolation
   - Status: Protected by QueryBuilder design

4. **Unauthorized schema changes**
   - Mitigation: Validating webhook + CRD RBAC (only cluster admins can apply SchemaDefinition)
   - Status: Protected by k8s apiserver RBAC

5. **Audit log tampering**
   - Mitigation: Hash-linked chain; tampering detected by `audit verify`
   - Status: Protected by ADR-005 (append-only, stored procedure)

6. **Actor impersonation**
   - Mitigation: JWT verification against JWKS; API key hash comparison; revocation list
   - Status: Protected by ADR-003 (fail-closed defaults)

7. **Replay attacks**
   - Mitigation: Idempotency keys stored with request hash; duplicate detected on replay
   - Status: Protected by ADR / U2

## ADR-007: Non-Superuser Database Role (RLS as Backstop)

This is the foundational security control. The `velocity_api` connection role:

- Is **not** a superuser
- Has `NOBYPASSRLS = true` (RLS cannot be bypassed)
- Only has GRANT'd permissions on specific tables
- Cannot create or drop tables, roles, or modify RLS policies

This means even if the API server is fully compromised (arbitrary code execution), an attacker cannot:
- Query rows outside their scope
- See other users' data
- Create new tables to exfiltrate data
- Drop tables or the audit log
- Bypass row-level security

### How RLS Works in Velocity

Every table has RLS policies like:

```sql
CREATE POLICY acme_supply_chain_procurement_policy
ON acme_supply_chain_procurement.purchase_order_v1
FOR SELECT
USING (
  -- User can see rows if they have a RoleBinding granting 'read' on this schema
  -- AND their scope (if any) matches the row's scope
  EXISTS (
    SELECT 1 FROM platform.role_bindings
    WHERE actor_id = current_setting('app.current_actor')
      AND schema_path = 'acme/supply-chain/procurement/purchase-order/v1'
      AND role IN ('procurement-reader', 'procurement-writer')
      AND (scope IS NULL OR scope @> hstore(scope_filter))
  )
);
```

On every request, the API server:

1. Sets `SET LOCAL app.current_actor = <jwt-actor-id>`
2. Sets `SET LOCAL app.current_region = <claim-value>` (if applicable)
3. Executes the query in a transaction with these settings

Postgres evaluates the RLS policy for every row. If the policy condition fails, the row is filtered out at the database level, not in app code.

### Verification Test

Connect directly to Postgres as `velocity_api`:

```sql
-- As velocity_api (non-superuser)
\c velocity velocity_api

-- Try to select from a table
SELECT * FROM acme_supply_chain_procurement.purchase_order_v1;
```

Expected results:
- If you are NOT in a RoleBinding for this schema: 0 rows
- If you ARE in a RoleBinding with `read: true`: rows within your scope
- You cannot see a `SELECT * FROM pg_roles` (no permission)

If you see all rows, RLS is not working correctly. Call your platform team.

## ADR-003: Authentication Fail Mode Matrix

When external dependencies (Redis, JWKS endpoint, etc.) are unavailable, the system must fail **safely** and **deterministically**. The default is deny (fail-closed).

### Revocation Check (Redis)

| Scenario | Default Behavior | Override |
|----------|------------------|----------|
| Redis unavailable | Deny request (503) | `failOpen: true` in AuthStrategy (dangerous) |
| Actor in revocation list | Deny request (401) | N/A |
| Actor not in revocation list | Allow request | N/A |

When Redis is down and `failOpen: false`, the request is denied. This is safe: better to deny legitimate requests than allow unauthorized ones.

```yaml
apiVersion: velocity.sh/v1
kind: AuthStrategy
metadata:
  name: jwt-internal
spec:
  type: jwt
  revocation:
    failOpen: false  # Dangerous if true
    checkInterval: 30s
```

### JWKS Cache (Issuer)

| Scenario | Behavior |
|----------|----------|
| JWKS endpoint unavailable, cache hit | Allow (signature verified against cached key) |
| JWKS endpoint unavailable, cache miss | Deny request (401) |
| JWKS endpoint unavailable, cache expired | Deny request (401) |

This is safe because we have a cryptographic guarantee: the token was signed by a key we previously verified.

### Audit Logging of Fail Modes

Every request logs which fail-mode was applied:

```sql
SELECT actor, operation, fail_mode, timestamp FROM platform.audit_log
WHERE fail_mode IS NOT NULL
ORDER BY timestamp DESC
LIMIT 10;
```

Expected output:

```
actor         operation fail_mode                   timestamp
───────────────────────────────────────────────────────────────
ravi.kumar    CREATE    REDIS_UNAVAILABLE_DENIED   2026-05-19 14:30:00
anita.sharma  READ      JWKS_CACHED_ALLOWED        2026-05-19 14:31:00
bot@acme.com  UPDATE    NONE                       2026-05-19 14:32:00
```

Set up alerts:

```yaml
# Prometheus
alert: AuthFailureModeActivated
expr: rate(velocity_auth_failmode[5m]) > 0
for: 5m
annotations:
  summary: "Auth fail-mode active ({{ $labels.fail_mode }})"
```

## ADR-001: Informer-per-Replica SchemaRegistry

The API server does not RPC to the operator for schema information. Instead, each API server replica runs a Kubernetes informer that watches the `SchemaDefinition` CRDs directly.

### Why This Matters for Security

- No single point of failure: if the operator is down, the API servers continue operating with cached schema state
- Decoupled from control plane: data plane resilience is not compromised
- Lock-free reads: schema lookups don't require locks (arc_swap; ADR-006)

### Verification

Check that the API server informer is ready:

```sh
kubectl logs -n velocity-system -l app=velocity-api | grep -i "informer.*ready"
```

Expected:

```json
{"level":"info", "message":"schema informer ready", "timestamp":"2026-05-19T14:32:00Z"}
```

The API server's `/readyz` endpoint returns 200 only when the informer has received its first full sync:

```sh
kubectl exec -n velocity-system deployment/velocity-api -- \
  curl http://localhost:8080/readyz
```

Response: `200 OK` (informer synced) or `503 Service Unavailable` (informer not ready).

## ADR-005: Append-Only Audit Chain

The audit log is immutable and hash-linked. Every entry includes:

- `event_id`: Incremental ID
- `event_hash`: SHA256 hash of this event + previous event hash (chain)
- `actor`: Who made the change
- `operation`: CREATE, UPDATE, DELETE, RESTORE
- `schema_path`: acme/supply-chain/procurement/purchase-order/v1
- `entity_id`: PO-00000001
- `old_value`: Previous state (JSON)
- `new_value`: New state (JSON)
- `timestamp`: When the change occurred
- `reason`: Why (if required by policy)

### Verify Chain Integrity

```sh
velocity audit verify --schema acme/supply-chain/procurement/purchase-order/v1 --id PO-00000001
```

Output:

```
✓ Audit chain valid (42 events, 0 tampering detected)
```

If tampering is detected:

```
✗ Audit chain invalid (hash mismatch at event 15)
  Event 14 computed hash: abc123... Event 15 expected: abc123... got: def456...
  This indicates the audit log was modified after the fact.
```

### Cryptographic Guarantee

The chain is recomputed by reading every event and verifying:

```
hash(event_N) = SHA256(event_N_data + hash(event_N-1))
```

If any field of any event is changed, the hash for that event changes, and the chain breaks at the next event. This is mathematically guaranteed.

## Validating Webhook: Last Line of Defence

The validating webhook runs before CRDs are persisted to etcd. It rejects manifests that:

1. Violate quota (e.g., too many SchemaDefinitions per Application)
2. Reference non-existent resources (e.g., AuthStrategy that doesn't exist)
3. Contain invalid CEL expressions (syntax error, depth > 10, size > 10 KB)
4. Attempt breaking schema changes without approval
5. Violate multi-tenancy boundaries (ADR-010: no cross-org refs in multi-tenant mode)

The webhook has a 5-second timeout. If it's unavailable, the default behavior is reject (fail-closed) unless explicitly configured otherwise (dangerous).

### Test Webhook Rejection

Attempt to apply a schema with a CEL syntax error:

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: bad-schema
spec:
  validation:
    rules:
      - rule: "self.amount >"  # Incomplete
        message: "Invalid"
```

Expected:

```
Error from server: error when creating "bad.yaml": admission webhook "schemadefinition.velocity.sh" denied the request: CEL parse error: unexpected token '>'
```

Attempt to apply a schema with an excessively deep CEL expression:

```yaml
spec:
  validation:
    rules:
      - rule: "((((((((((((((((((((((((((((((((((((((((((((((((((((((((((true))))))))))))))))))))))))))))))))))))))))))))))))))))))))))))"
        message: "Nested too deep"
```

Expected:

```
Error from server: error when creating "bad.yaml": admission webhook "schemadefinition.velocity.sh" denied the request: CEL nesting depth > 10
```

## Cross-Domain Joins and RBAC

When a query includes data from another schema (e.g., fetching supplier details alongside a purchase order), Layer 3 RBAC checks that the actor has `read` permission on the target schema.

**Example:** Actor has `procurement-reader` but not `supplier-reader`.

```sh
velocity record query \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --include supplier
```

Expected response:

```json
{"code": "CROSS_SCHEMA_ACCESS_DENIED", "message": "You do not have read access to acme/supply-chain/sourcing/supplier/v1"}
```

This is enforced in the API query builder before SQL is even constructed. An actor cannot accidentally leak data from a schema they don't have access to.

## Input Size Limits and DoS Prevention

### Request Body Size

Maximum 10 MB per request. Larger payloads are rejected with 413 Payload Too Large.

### Field Size

Each field has a `maxLength`. For example:

```yaml
fields:
  - name: notes
    type: string
    maxLength: 1000
```

Attempting to set a field to 10,000 characters will fail validation before the database is touched.

### CEL Execution Time

CEL expressions have a default 10ms timeout. Expressions that take longer are terminated:

```
CEL evaluation timeout: rule took 25ms (max 10ms)
```

### Query Complexity

Queries are limited to:
- 2-level joins (prevent runaway graph traversal)
- 1000 result limit (cursor pagination required for larger sets)

### Search Query Cardinality

Full-text search queries must match the schema's search tier:
- Tier 1 (trigram): Fast, shallow searches
- Tier 2 (FTS): Slower, more expressive
- Tier 3 (Typesense): Real-time, typo-tolerant

Queries that would return > 10,000 results are rejected to prevent resource exhaustion.

## Secrets and Credentials

### API Keys

Stored as SHA256 hashes. Plaintext is shown once at creation:

```sh
velocity api-key create --name prod --ttl 30d
```

The CLI never logs or repeats the plaintext.

### Database Passwords

Should be stored in a secret manager (HashiCorp Vault, AWS Secrets Manager, Sealed Secrets). Never hardcode or commit to git.

```sh
# Never do this:
kubectl create secret generic velocity-postgres-creds \
  --from-literal=password='mysecretpassword'

# Instead:
kubectl apply -f secret-from-vault.yaml
# or use External Secrets Operator
```

## Compliance and Audit

### Structured Logging

All logs are JSON. This makes them machine-parseable for log aggregation and audit:

```json
{"level": "info", "timestamp": "2026-05-19T14:32:00Z", "actor": "ravi.kumar", "operation": "CREATE", "schema": "acme/supply-chain/procurement/purchase-order/v1", "entity_id": "PO-00000001", "status": "success"}
```

### Metrics

Prometheus metrics are exported:

```
velocity_operations_total{operation="create", schema="acme/supply-chain/procurement/purchase-order/v1", outcome="success"} 1542
velocity_auth_failures_total{strategy="jwt", reason="invalid_signature"} 3
velocity_audit_chain_tampering_detected_total 0
velocity_rls_policy_denied_total 42
```

Set up alerting on tampering:

```yaml
alert: AuditTamperingDetected
expr: increase(velocity_audit_chain_tampering_detected_total[5m]) > 0
for: 1m
annotations:
  summary: "Audit chain tampering detected"
  severity: critical
```

## Zero-Trust Principles

Velocity implements zero-trust:

1. **No implicit trust:** Every request must authenticate
2. **Default deny:** Reject unless explicitly allowed
3. **Cryptographic verification:** JWT signatures, API key hashes, audit chain integrity
4. **Defense in depth:** 7 layers of access control
5. **Auditability:** Every operation recorded and verifiable
6. **Least privilege:** Non-superuser role, per-schema roles, scoped row filters

## Next Steps

- **[Hardening](./hardening)** — Production checklist
- **[Troubleshooting](./troubleshooting)** — Resolve security-related issues
- **[Architecture Decisions](./adrs)** — Deep dive into each ADR
