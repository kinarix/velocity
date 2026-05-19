---
title: Troubleshooting
description: Common issues and their solutions
---

## API Returns 503 REVOCATION_UNAVAILABLE

**Symptom:**

```json
{"code": "REVOCATION_UNAVAILABLE", "message": "Revocation check unavailable"}
```

**Root cause:** Redis is unreachable, and the AuthStrategy is configured with `failOpen: false` (the default, which is safe).

**Solution:**

1. Check Redis health:
   ```sh
   kubectl get pod -n velocity-system -l app=redis
   ```

2. If Redis pod is not running, restart it:
   ```sh
   kubectl rollout restart statefulset/redis -n velocity-system
   ```

3. Check Redis logs for errors:
   ```sh
   kubectl logs -n velocity-system -l app=redis --tail=100
   ```

4. Verify network connectivity from API pod to Redis:
   ```sh
   kubectl exec -n velocity-system deployment/velocity-api -- \
     redis-cli -h redis:6379 ping
   ```

   Expected: `PONG`

5. If the issue persists, check your AuthStrategy configuration:
   ```yaml
   apiVersion: velocity.sh/v1
   kind: AuthStrategy
   metadata:
     name: jwt-internal
   spec:
     revocation:
       failOpen: false  # This is correct (safe default)
   ```

   To allow requests when Redis is down (dangerous), set `failOpen: true`. Only do this in non-production environments.

## API Returns 401 Invalid Bearer Token

**Symptom:**

```json
{"code": "AUTH_INVALID_TOKEN", "message": "Invalid or expired token"}
```

**Root cause:**
- Token is expired
- Token signature is invalid
- Token issuer is not configured in AuthStrategy
- JWKS endpoint returned a key that doesn't match

**Solution:**

1. Check token expiration:
   ```sh
   # Decode the JWT (base64 decode the middle section)
   echo <token> | awk -F'.' '{print $2}' | base64 -d | jq .
   ```

   Look for `"exp"` field. If `exp < now()`, the token is expired.

2. Verify AuthStrategy is configured for your issuer:
   ```sh
   kubectl get authstrategy -A
   kubectl describe authstrategy jwt-internal -n velocity-system
   ```

   Check that `spec.issuer` matches your token's `iss` claim.

3. Test token verification:
   ```sh
   curl -H "Authorization: Bearer <token>" \
     https://api.velocity.acme.com/version
   ```

   If it returns 401, the token is invalid. If it returns 200, auth is working.

4. Check JWKS cache:
   ```sh
   kubectl logs -n velocity-system -l app=velocity-api | grep -i jwks
   ```

   Should show periodic refreshes every 5 minutes. If you see errors, the JWKS endpoint may be down.

## Reconciler Hot-loops / Operator CPU High

**Symptom:**

- Operator pod CPU usage > 50% continuously
- Logs show repeated reconciliation of the same CRD:
  ```json
  {"message": "reconciling", "schema": "acme/supply-chain/procurement/purchase-order/v1", "attempt": 1234}
  ```

**Root cause:** Schema has drifted from its spec. The operator reconciles, makes changes, detects a diff, reconciles again, etc.

**Solution:**

1. Check the operator logs for the error:
   ```sh
   kubectl logs -n velocity-system -l app=velocity-operator --tail=200 | \
     grep -A5 "error"
   ```

2. Common causes:
   - Postgres table has uncommitted changes (e.g., a manual `ALTER TABLE` by DBA)
   - Operator role lacks permission to create/modify a resource
   - A required RLS policy is missing
   - Archive or history table is out of sync

3. Run the drift check:
   ```sh
   velocity drift check \
     --schema acme/supply-chain/procurement/purchase-order/v1
   ```

   This reports what is drifted.

4. Quarantine the schema to stop reconciliation:
   ```sh
   kubectl annotate schemadefinition purchase-order \
     -n acme-supply-chain-procurement \
     velocity.sh/quarantine=true \
     --overwrite
   ```

5. Fix the drift manually (with DBA):
   ```sql
   -- Example: add a missing RLS policy
   CREATE POLICY acme_supply_chain_procurement_policy
   ON acme_supply_chain_procurement.purchase_order_v1
   FOR SELECT USING (...);
   ```

6. Remove the quarantine:
   ```sh
   kubectl annotate schemadefinition purchase-order \
     -n acme-supply-chain-procurement \
     velocity.sh/quarantine- \
     --overwrite
   ```

7. Trigger a manual reconcile:
   ```sh
   velocity reconcile --schema acme/supply-chain/procurement/purchase-order/v1
   ```

## Outbox Table Grows Unbounded

**Symptom:**

```sql
SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1_outbox
WHERE published_at IS NULL;
```

Returns thousands of unpublished rows.

**Root cause:** CDC worker is not running or has crashed. Outbox rows are not being sent to Typesense.

**Solution:**

1. Check CDC worker health:
   ```sh
   kubectl logs -n velocity-system -l app=velocity-api -c cdc-worker --tail=100
   ```

   Look for `panic`, `error`, or connection issues.

2. Verify Typesense is reachable:
   ```sh
   kubectl exec -n velocity-system deployment/velocity-api -- \
     curl https://typesense.example.com:8108/health
   ```

   Should return status 200 with health info.

3. Check if the CDC worker is running:
   ```sh
   kubectl exec -n velocity-system deployment/velocity-api -- \
     ps aux | grep cdc
   ```

   Should show a running process.

4. Restart the API deployment:
   ```sh
   kubectl rollout restart deployment/velocity-api -n velocity-system
   ```

5. Monitor the outbox shrinking:
   ```sh
   watch -n 5 'psql -U velocity_api -d velocity -c "SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1_outbox WHERE published_at IS NULL;"'
   ```

   The count should decrease as rows are processed.

6. If the count doesn't shrink, check Typesense logs:
   ```sh
   kubectl logs -n typesense -l app=typesense --tail=100
   ```

   The collection may be read-only, or there may be a permission issue.

## Schema Apply Succeeds but Table Not Created

**Symptom:**

```sh
kubectl apply -f schema.yaml
# Output: schemadefinition.velocity.sh/purchase-order created
# Status shows "Ready"

# But the table doesn't exist:
psql -U velocity_api -d velocity -c \
  "SELECT to_regclass('acme_supply_chain_procurement.purchase_order_v1');"
```

Returns `NULL`.

**Root cause:** Operator does not have permission to CREATE TABLE in the domain schema.

**Solution:**

1. Check operator logs:
   ```sh
   kubectl logs -n velocity-system -l app=velocity-operator --tail=200 | \
     grep -i "permission\|denied"
   ```

2. Verify operator's Postgres role has CREATE permission:
   ```sql
   -- As superuser
   SELECT * FROM information_schema.role_table_grants
   WHERE grantee = 'velocity_operator' AND table_schema LIKE 'acme_%';
   ```

   Should show `CREATE` in the privileges.

3. Grant the permission:
   ```sql
   -- As superuser
   GRANT CREATE ON SCHEMA acme_supply_chain_procurement TO velocity_operator;
   ```

4. Retry the apply:
   ```sh
   velocity reconcile --schema acme/supply-chain/procurement/purchase-order/v1
   ```

## Time Machine Shows Empty History

**Symptom:**

```sh
velocity history list --schema ... --id PO-00000001
```

Returns 0 events, even though records were created and updated.

**Root cause:** History trigger is not firing, or history table is missing.

**Solution:**

1. Check if the history table exists:
   ```sql
   SELECT to_regclass('acme_supply_chain_procurement.purchase_order_v1_history');
   ```

   If NULL, the operator did not create it. Reconcile the schema:
   ```sh
   velocity reconcile --schema acme/supply-chain/procurement/purchase-order/v1
   ```

2. Check if the trigger exists:
   ```sql
   SELECT trigger_name FROM information_schema.triggers
   WHERE event_object_table = 'purchase_order_v1'
     AND trigger_schema = 'acme_supply_chain_procurement';
   ```

   Should show a trigger like `purchase_order_v1_history_trigger`.

3. Check if the trigger is enabled:
   ```sql
   SELECT tgenabled FROM pg_trigger
   WHERE relname = 'purchase_order_v1'
     AND tgname = 'purchase_order_v1_history_trigger';
   ```

   Should return `O` (enabled). If it returns `D`, the trigger is disabled:
   ```sql
   -- Enable it
   ALTER TABLE acme_supply_chain_procurement.purchase_order_v1 ENABLE ALWAYS TRIGGER purchase_order_v1_history_trigger;
   ```

4. Manually verify the trigger works by creating a test record:
   ```sh
   velocity record create \
     --schema acme/supply-chain/procurement/purchase-order/v1 \
     --data '{"id": "TEST-001", "status": "draft"}'
   ```

5. Check the history table:
   ```sql
   SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1_history
   WHERE entity_id = 'TEST-001';
   ```

   Should show 1 (the CREATE event).

## Archive Worker Silently Skips Policy

**Symptom:**

- ArchivePolicy is applied and shows Ready
- Archive worker pod is running
- No errors in logs
- But records are not being archived to S3

**Root cause:**
1. Archive policy type is `cel` (deferred to Phase 10)
2. S3 destination is not configured
3. `ARCHIVE_S3_BUCKET` environment variable is not set

**Solution:**

1. Check the archive policy:
   ```sh
   kubectl get archivepolicy -A
   kubectl describe archivepolicy <name>
   ```

   Look at `spec.trigger.type`. If it's `cel`, this is deferred:
   ```yaml
   spec:
     trigger:
       type: cel  # Deferred; only 'age' and 'size' are implemented
   ```

2. Use `age` or `size` instead:
   ```yaml
   spec:
     trigger:
       type: age
       days: 30
   ```

3. Verify S3 bucket is configured:
   ```sh
   kubectl get deployment velocity-archive-worker -n velocity-system -o yaml | \
     grep -A5 "ARCHIVE_S3"
   ```

4. If not set, update the Helm values:
   ```yaml
   archiveWorker:
     env:
       ARCHIVE_S3_BUCKET: velocity-archives
       ARCHIVE_S3_REGION: us-east-1
   ```

5. Reapply Helm:
   ```sh
   helm upgrade velocity velocity/velocity \
     --namespace velocity-system \
     --values values.yaml
   ```

6. Check archive run status:
   ```sql
   SELECT schema_path, status, started_at, completed_at FROM platform.archive_runs
   ORDER BY started_at DESC LIMIT 10;
   ```

   Should show recent runs with status `success`.

## velocity context add Fails with "Invalid Bearer Token"

**Symptom:**

```sh
velocity context add \
  --name prod \
  --api-url https://api.velocity.acme.com \
  --bearer-token eyJhbGc...
```

Error:

```
Error: invalid bearer token (contains CRLF)
```

**Root cause:** The token contains newlines (common when copying from email or docs).

**Solution:**

1. Remove newlines from the token:
   ```sh
   TOKEN=$(cat token.txt | tr -d '\n\r')
   velocity context add --name prod --api-url ... --bearer-token "$TOKEN"
   ```

2. Verify the token is valid:
   ```sh
   echo "$TOKEN" | awk -F'.' '{if(NF!=3) exit 1}' && echo "Valid JWT structure"
   ```

3. Retry the context add.

## velocity api-key create Times Out Waiting for Secret

**Symptom:**

```sh
velocity api-key create --name prod --ttl 30d
```

Hangs for 5+ minutes, then times out:

```
Error: timeout waiting for secret to appear
```

**Root cause:** Operator is not running, or API server cannot create Kubernetes secrets.

**Solution:**

1. Check if operator is running:
   ```sh
   kubectl get deployment velocity-operator -n velocity-system
   ```

   Should show 1 replica.

2. Check operator logs:
   ```sh
   kubectl logs -n velocity-system -l app=velocity-operator --tail=100
   ```

   Look for FATAL or ERROR.

3. Check API server RBAC:
   ```sh
   kubectl describe rolebinding velocity-api -n velocity-system
   ```

   Should show permission to create secrets.

4. If RBAC is missing, add it:
   ```yaml
   apiVersion: rbac.authorization.k8s.io/v1
   kind: Role
   metadata:
     name: velocity-api
     namespace: velocity-system
   rules:
     - apiGroups: [""]
       resources: ["secrets"]
       verbs: ["get", "list", "watch", "create", "update", "patch"]
   ```

5. Retry with a longer timeout:
   ```sh
   velocity api-key create \
     --name prod \
     --ttl 30d \
     --wait-secs 120
   ```

## API Server Crashes with "RLS will not work"

**Symptom:**

API server logs show:

```
FATAL velocity_api role has BYPASSRLS=true — RLS will not work. Fix the role.
```

Then the pod restarts continuously.

**Root cause:** The database role `velocity_api` has `BYPASSRLS=true` (allowing it to bypass RLS).

**Solution:**

1. Connect to Postgres as superuser:
   ```sh
   psql -h <pg-host> -U <superuser> -d velocity
   ```

2. Fix the role:
   ```sql
   ALTER ROLE velocity_api NOBYPASSRLS;
   ```

3. Verify:
   ```sql
   SELECT rolname, rolbypassrls FROM pg_roles WHERE rolname = 'velocity_api';
   ```

   Should show `rolbypassrls = f`.

4. Restart the API deployment:
   ```sh
   kubectl rollout restart deployment/velocity-api -n velocity-system
   ```

5. Tail logs to verify startup:
   ```sh
   kubectl logs -f -n velocity-system -l app=velocity-api
   ```

   Should show "schema informer ready" when healthy.

## Validating Webhook Rejects Valid Manifest

**Symptom:**

Applying a manifest fails with:

```
Error from server: error when creating "schema.yaml": admission webhook "schemadefinition.velocity.sh" denied the request: ...
```

But the manifest looks correct.

**Solution:**

1. Get more details:
   ```sh
   kubectl apply -f schema.yaml -v=6 2>&1 | grep -A10 "denied"
   ```

2. Common webhook rejections:
   - **Namespace mismatch:** `{org}-{app}-{domain}` namespace must match CRD metadata
     ```yaml
     # Must be in namespace acme-supply-chain-procurement
     metadata:
       namespace: acme-supply-chain-procurement
       labels:
         velocity.sh/org: acme
         velocity.sh/app: supply-chain
         velocity.sh/domain: procurement
     ```

   - **CEL syntax error:** Fix the rule
     ```yaml
     validation:
       rules:
         - rule: "self.amount >"  # Missing right side
     ```

   - **Quota exceeded:** Too many SchemaDefinitions in the Application
     ```sql
     SELECT COUNT(*) FROM platform.schema_definitions
     WHERE org = 'acme' AND app = 'supply-chain';
     ```

     If you need more, update the quota in the Application CRD.

   - **Cross-org reference (multi-tenant):** In multi-tenant mode, you cannot reference a schema from another org
     ```yaml
     # This is forbidden in multi-tenant:
     spec:
       refs:
         - path: other-org/app/domain/schema/v1
     ```

3. Check webhook configuration:
   ```sh
   kubectl get validatingwebhookconfigurations | grep velocity
   kubectl describe validatingwebhookconfigurations velocity.sh-schemadefinition
   ```

4. If the webhook is misconfigured, delete and restart:
   ```sh
   kubectl rollout restart deployment/velocity-webhook -n velocity-system
   ```

## Next Steps

If you can't find your issue here, check:

- **[Security](./security)** — Auth and RLS-related issues
- **[Hardening](./hardening)** — CEL, input validation
- **[API Reference](./api-reference)** — REST endpoint issues
- **Operator logs:** `kubectl logs -f -n velocity-system -l app=velocity-operator`
- **API logs:** `kubectl logs -f -n velocity-system -l app=velocity-api`

Or reach out to the team.
