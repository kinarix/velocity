---
title: Hardening
description: Production security checklist for Velocity deployments
---

Before deploying Velocity to production, verify every item on this checklist. Each reflects a load-bearing security decision in CLAUDE.md or an ADR.

## Database Role (ADR-007)

### Non-Superuser Role

The `velocity_api` connection role must be non-superuser with `NOBYPASSRLS=true`. This is the primary backstop against unauthorized data access.

**Verification:**

```sql
SELECT 
  rolname,
  rolbypassrls as "RLS Enforced",
  rolinherit as "Can Inherit"
FROM pg_roles 
WHERE rolname = 'velocity_api';
```

Expected output:

```
 rolname    | RLS Enforced | Can Inherit
────────────┼──────────────┼───────────
 velocity_api|     f        |     t
```

- `RLS Enforced = f` (false): RLS cannot be bypassed
- `Can Inherit = t` (true): Role can inherit from other roles

**If misconfigured:** The API server startup will log:

```
FATAL velocity_api role has BYPASSRLS=true — RLS will not work.
```

Fix and restart:

```sql
ALTER ROLE velocity_api NOBYPASSRLS;
```

```sh
kubectl rollout restart deployment/velocity-api -n velocity-system
```

### Per-Domain Roles

The operator creates per-domain roles (e.g., `acme_supply_chain_procurement_role`) with specific table grants. Verify they exist:

```sql
SELECT * FROM pg_roles WHERE rolname LIKE '%_role';
```

Each row represents a domain's RBAC boundary.

## Input Validation

### Request Body Size Limit

The API defaults to **10 MB per request**. Verify in the Axum middleware:

```rust
.layer(DefaultBodyLimit::max(10 * 1024 * 1024))
```

Attempting to POST a payload larger than 10 MB will return 413 Payload Too Large.

### Field-Level Size Limits

Each field definition must specify `maxLength` or be enforced in validation rules. For example:

```yaml
spec:
  fields:
    - name: notes
      type: string
      maxLength: 1000  # Required
```

Attempting to set `notes: "X" * 10000` will fail validation.

**Check deployed schemas:**

```sh
kubectl get schemadefs -A -o json | \
  jq '.items[] | select(.spec.fields[] | .maxLength == null) | .metadata.name'
```

Any non-null results indicate schemas that accept unbounded strings. Add `maxLength` and reapply.

### CEL Expression Safety (Deferred to Phase 2b Details)

CEL expressions used in validation rules are compiled at schema-load time and executed with a **10ms timeout per rule**.

**Verification:**

```yaml
spec:
  validation:
    rules:
      - rule: "self.amount > 0 && self.currency in ['USD', 'EUR']"
        message: "Invalid amount or currency"
        maxExecutionMs: 10  # Optional; defaults to 10
```

Any rule taking longer than `maxExecutionMs` (or 10ms default) is terminated and the request is denied.

**Test CEL safety:**

```sh
# Apply a schema with a deliberately slow rule (infinite loop simulation)
velocity apply -f schema-with-slow-cel.yaml

# Attempt to create a record
velocity record create --schema ... --data ...

# Expected: 400 Bad Request with message "CEL execution timeout"
```

## API Key Security

### SHA256 Storage

API keys are stored as SHA256 hashes in the database. Plaintext is shown **once at creation**:

```sh
velocity api-key create --name prod-secret --ttl 90d
```

Output:

```
Name: prod-secret
Key:  vel_prod_xxxxxxxxxxxxxx...  [NEVER SHARED AGAIN]
```

**Verify in database:**

```sql
SELECT name, key_hash, created_at, expires_at FROM platform.api_keys;
```

All entries in `key_hash` column should be 64-character hex strings (SHA256), never plaintext.

### Expiration and Rotation

Create short-lived keys:

```sh
velocity api-key create --name deploy-ci --ttl 30d
```

Automate rotation:

```sh
# In a cron job or scheduled agent
velocity api-key list --expired | xargs -I {} velocity api-key revoke {}
```

## Sensitive Field Redaction

Fields marked with `sensitivity` levels should never appear in logs or API responses unmasked.

**Schema definition:**

```yaml
spec:
  fields:
    - name: credit_card
      type: string
      sensitivity: pii
      masking:
        strategy: partial
        visibleChars: 4  # Show last 4 digits only
```

**Verify in logs:**

```sh
kubectl logs -n velocity-system -l app=velocity-api | \
  jq 'select(.payload | contains("credit_card"))' | head -5
```

Logs should either:
1. Omit the field entirely
2. Show only the masked portion (e.g., `credit_card: "****1234"`)

Never show plaintext sensitive fields.

## Audit Trail Requirements

Every mutation must be recorded in the immutable audit log. Verify the audit stored procedure exists:

```sql
SELECT routine_name, routine_type FROM information_schema.routines 
WHERE routine_name = 'audit_insert' AND routine_schema = 'platform';
```

Expected:

```
 routine_name | routine_type
──────────────┼──────────────
 audit_insert | PROCEDURE
```

### Audit Chain Integrity

The audit log is hash-linked. Verify chain integrity:

```sh
velocity audit verify --schema acme/supply-chain/procurement/purchase-order/v1
```

Output:

```
✓ Audit chain valid (1534 events, 0 tampering detected)
```

If tampering is detected, immediate action is required:

```
✗ Audit chain invalid (hash mismatch at event 42)
  Event 42: expected hash abc123... got def456...
  This indicates the audit log was modified.
```

Set up monitoring:

```yaml
# Prometheus alert
alert: AuditChainTampered
expr: velocity_audit_tampering_detected > 0
for: 5m
annotations:
  summary: "Audit chain tampering detected"
  action: "Quarantine schema, contact security team"
```

## Validating Webhook

The validating webhook is the last line of defence before CRDs are accepted. Verify it is running:

```sh
kubectl get validatingwebhookconfigurations | grep velocity
```

Should list:

```
velocity.sh-schemadefinition
velocity.sh-authstrategy
velocity.sh-archivepolicy
...
```

### Test Rejection

Attempt to apply an invalid schema (e.g., CEL expression with infinite loop):

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: bad-schema
  namespace: acme-test
spec:
  # ... fields ...
  validation:
    rules:
      - rule: "true while true"  # Invalid syntax
```

Expected error:

```
Error from server: error when creating "bad.yaml": admission webhook "schemadefinition.velocity.sh" denied the request: CEL syntax error: ...
```

### Webhook Availability

If the webhook is unavailable, CRD applies will be rejected by default (unless explicitly configured to failOpen, which is dangerous). Check webhook pod health:

```sh
kubectl logs -n velocity-system -l app=velocity-webhook --tail=50
```

All logs should be normal; no "connection refused" errors.

## Outbox Writes for Tier-3 Search

If `search.tier: 3` (Typesense) is enabled, outbox table writes are mandatory for consistency. Every mutation writes to both the main table and the outbox in a single transaction.

**Verify outbox table exists:**

```sql
SELECT table_name FROM information_schema.tables 
WHERE table_schema = 'acme_supply_chain_procurement' 
  AND table_name LIKE '%_outbox';
```

Should list:

```
purchase_order_v1_outbox
```

### Monitor CDC Worker

The CDC worker processes outbox rows and syncs to Typesense. Monitor lag:

```sh
# Check unpublished outbox rows
psql -U velocity_api -d velocity -c \
  "SELECT COUNT(*) as outbox_lag FROM acme_supply_chain_procurement.purchase_order_v1_outbox 
   WHERE published_at IS NULL;"
```

If lag grows unbounded, the CDC worker may have crashed:

```sh
kubectl logs -n velocity-system -l app=velocity-api -c cdc-worker --tail=100
```

Restart the API deployment if needed:

```sh
kubectl rollout restart deployment/velocity-api -n velocity-system
```

## Authentication Fail Modes (ADR-003)

### Redis Revocation Check (Deny by Default)

If Redis is unreachable during a request, the default is to **deny** the request (fail-closed). This is the secure default.

**To verify fail mode:**

1. Stop Redis:
   ```sh
   kubectl scale statefulset redis --replicas=0
   ```

2. Attempt a request:
   ```sh
   velocity record list --schema acme/supply-chain/procurement/purchase-order/v1
   ```

3. Expected: `503 Service Unavailable` with code `REVOCATION_UNAVAILABLE`.

4. Check audit log for fail-mode recorded:
   ```sql
   SELECT actor, operation, fail_mode FROM platform.audit_log 
   WHERE fail_mode IS NOT NULL 
   ORDER BY created_at DESC LIMIT 5;
   ```

### Typesense Search (Degrade to Tier 2)

If Typesense is unreachable during a **search** request (not a mutation), the system falls back to Postgres FTS (Tier 2). This is acceptable because search is read-only.

**To verify degradation:**

1. Stop Typesense:
   ```sh
   kubectl scale deployment typesense --replicas=0
   ```

2. Attempt a search:
   ```sh
   velocity record query --schema acme/supply-chain/procurement/purchase-order/v1
   ```

3. Expected: 200 OK with results from Postgres FTS.

4. Check logs:
   ```sh
   kubectl logs -n velocity-system -l app=velocity-api | \
     grep -i "typesense.*unavailable"
   ```

5. Restore Typesense:
   ```sh
   kubectl scale deployment typesense --replicas=1
   ```

## Breaking Schema Changes

Removing a field, changing a type, or renaming a column are **breaking changes** that could corrupt data or break existing clients. These are blocked unless explicitly approved.

**To apply a breaking change:**

1. Add the approval annotation:
   ```yaml
   metadata:
     annotations:
       velocity.sh/breaking-change: "approved"
   ```

2. Include a reason:
   ```yaml
   spec:
     migrations:
       - type: DropColumn
         column: deprecated_field
         reason: "Field unused since Q1 2026; 0 records have values"
   ```

3. Apply:
   ```sh
   kubectl apply -f schema-with-breaking-change.yaml
   ```

Expected: The operator accepts and logs the migration with a timestamp and the provided reason.

## RLS Enforcement Verification

Row-level security is enforced by Postgres for every query, even if the application layer has bugs. Verify this directly:

```sh
# Connect as velocity_api (non-superuser)
psql -h <pg-host> -U velocity_api -d velocity

# Try to select from acme.supply-chain.procurement purchase-order table
SELECT * FROM acme_supply_chain_procurement.purchase_order_v1;
```

Expected behavior:
- If you have a RoleBinding granting `read` on the schema, you see rows your scope allows
- If you don't have a RoleBinding, you see 0 rows (not a permission denied error; just no data)

If you can select all rows despite not having a RoleBinding, RLS is not properly configured.

## TLS for Internal Communication

All inter-service communication should be encrypted:

```sh
# Verify API → Postgres connection uses SSL
kubectl logs -n velocity-system -l app=velocity-api | grep "sslmode"
# Should show: sslmode=require

# Verify API → Typesense connection uses HTTPS
kubectl logs -n velocity-system -l app=velocity-api | grep "typesense.*https"
```

## Network Policies

Restrict traffic to Velocity pods:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: velocity-api
  namespace: velocity-system
spec:
  podSelector:
    matchLabels:
      app: velocity-api
  policyTypes:
    - Ingress
  ingress:
    - from:
        - namespaceSelector:
            matchLabels:
              name: ingress-nginx  # Allow from ingress controller
      ports:
        - protocol: TCP
          port: 8080
    - from:
        - podSelector:
            matchLabels:
              app: velocity-webhook  # Allow from webhook
      ports:
        - protocol: TCP
          port: 8080
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: velocity-egress
  namespace: velocity-system
spec:
  podSelector:
    matchLabels:
      app: velocity-api
  policyTypes:
    - Egress
  egress:
    # Allow DNS
    - to:
        - namespaceSelector: {}
      ports:
        - protocol: UDP
          port: 53
    # Allow Postgres
    - to:
        - podSelector:
            matchLabels:
              app: postgres
      ports:
        - protocol: TCP
          port: 5432
    # Allow Redis
    - to:
        - podSelector:
            matchLabels:
              app: redis
      ports:
        - protocol: TCP
          port: 6379
    # Allow Typesense
    - to:
        - podSelector:
            matchLabels:
              app: typesense
      ports:
        - protocol: TCP
          port: 8108
    # Allow external HTTPS (for JWKS fetch, S3, etc.)
    - to:
        - namespaceSelector: {}
      ports:
        - protocol: TCP
          port: 443
```

Apply:

```sh
kubectl apply -f network-policies.yaml
```

## Pod Security Policy / Pod Security Standards

Enforce restricted security policies:

```yaml
apiVersion: policy/v1beta1
kind: PodSecurityPolicy
metadata:
  name: velocity-restricted
spec:
  privileged: false
  allowPrivilegeEscalation: false
  requiredDropCapabilities:
    - ALL
  volumes:
    - 'configMap'
    - 'emptyDir'
    - 'projected'
    - 'secret'
    - 'downwardAPI'
    - 'persistentVolumeClaim'
  runAsUser:
    rule: 'MustRunAsNonRoot'
  seLinux:
    rule: 'MustRunAs'
    seLinuxOptions:
      level: "s0:c123,c456"
  readOnlyRootFilesystem: true
```

Or use Pod Security Standards (PSS) labels on the namespace:

```sh
kubectl label namespace velocity-system \
  pod-security.kubernetes.io/enforce=restricted \
  pod-security.kubernetes.io/audit=restricted \
  pod-security.kubernetes.io/warn=restricted
```

## Secrets Management

Do not store secrets in ConfigMaps or hardcoded in manifests. Use:

- Kubernetes Secrets (base64 encoded; at-rest encryption via KMS)
- External Secrets Operator (fetch from AWS Secrets Manager, HashiCorp Vault, etc.)
- Sealed Secrets (bitnami/sealed-secrets)

Example with External Secrets:

```yaml
apiVersion: external-secrets.io/v1beta1
kind: SecretStore
metadata:
  name: aws-secret-store
  namespace: velocity-system
spec:
  provider:
    aws:
      service: SecretsManager
      region: us-east-1
      auth:
        jwt:
          serviceAccountRef:
            name: velocity-secrets

---
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: velocity-postgres-creds
  namespace: velocity-system
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: aws-secret-store
    kind: SecretStore
  target:
    name: velocity-postgres-creds
    creationPolicy: Owner
  data:
    - secretKey: superuser-password
      remoteRef:
        key: velocity/postgres/superuser-password
    - secretKey: velocity-api-password
      remoteRef:
        key: velocity/postgres/velocity-api-password
```

## Audit and Compliance

### Enable Audit Logging

Kubernetes audit logging captures all API requests:

```yaml
# kube-apiserver flag
--audit-log-path=/var/log/kubernetes/audit/audit.log
--audit-policy-file=/etc/kubernetes/audit-policy.yaml
```

### Backup Strategy

Verify Postgres backup is running:

```sh
# Check CNPG backup status
kubectl get cnpg velocity-postgres -n velocity-system -o wide
```

Should show:

```
NAME                INSTANCES READY STATUS                  POSTGRESQL    HASH
velocity-postgres   3         3     Cluster in healthy      15.2          ...
```

Check S3 bucket has backups:

```sh
aws s3 ls s3://velocity-backups/postgres/ --recursive | head -10
```

## Checklist

- [ ] `velocity_api` role is non-superuser with `NOBYPASSRLS=true`
- [ ] All SchemaDefinitions have `maxLength` on string fields
- [ ] CEL expressions have `maxExecutionMs` set (default 10ms)
- [ ] API key hashes are stored, not plaintext
- [ ] Sensitive fields use masking strategies
- [ ] Audit log is hash-linked; `audit verify` passes
- [ ] Validating webhook is running and rejecting invalid manifests
- [ ] Outbox tables exist for Tier-3 schemas; CDC worker is healthy
- [ ] Redis unavailability causes 503 (fail-closed), not silent failures
- [ ] Typesense unavailability degrades gracefully to Tier 2
- [ ] Breaking schema changes require `velocity.sh/breaking-change=approved`
- [ ] RLS verified by direct Postgres query (non-superuser sees scoped rows only)
- [ ] Internal communication uses TLS/HTTPS
- [ ] Network policies restrict pod-to-pod traffic
- [ ] Pod security policies enforce non-root, no-escalation
- [ ] Secrets use external secret manager or Sealed Secrets
- [ ] Postgres backups are archiving to S3 regularly
- [ ] Audit log monitoring and alerting is configured
- [ ] Operator logs show no FATAL or ERROR on startup

Once every item is verified, your Velocity deployment is production-ready.

## Next Steps

- **[Security](./security)** — Threat model and deeper RLS story.
- **[Troubleshooting](./troubleshooting)** — Resolve common issues.
- **[Operations](./runbooks)** — Backup, restore, and incident response.
