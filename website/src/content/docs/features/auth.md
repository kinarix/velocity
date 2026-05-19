---
title: Authentication & Authorization
description: JWT, OIDC, API keys, and 7-layer access control
---

Velocity supports multiple authentication strategies (JWT, OIDC, API key, Composite) and enforces access control through 7 layers.

## Authentication Strategies

### AuthStrategy CRD

```yaml
apiVersion: velocity.sh/v1
kind: AuthStrategy
metadata:
  name: jwt-internal
  namespace: platform
spec:
  type: jwt  # jwt | oidc | api_key | composite
  displayName: "Internal JWT"
  
  jwt:
    issuer: https://auth.acme.com
    audience: acme-api
    
    # Multiple issuers
    issuers:
      - issuer: https://auth.acme.com
        audience: acme-api
      - issuer: https://idp.partners.com
        audience: acme-partner-api
    
    jwksUrl: https://auth.acme.com/.well-known/jwks.json
    claimMapping:
      actorId: sub
      roles:
        - path: realm_access.roles
          transform: identity
      attributes:
        region:
          path: custom_claims.region
          transform: identity
        store_id:
          path: custom_claims.store_id
          transform: identity
    
    revocation:
      failOpen: false  # Default deny when Redis unavailable

  oidc:
    clientId: velocity-app
    clientSecret: <secret-ref>
    discoveryUrl: https://idp.example.com/.well-known/openid-configuration
    redirectUri: https://api.velocity.acme.com/auth/callback
    scopes: [openid, profile, email]
    claimMapping:
      actorId: sub
      roles:
        - path: groups
          transform: identity
    
    sessionTtl: 8h
    sessionSecure: true

  apiKey:
    headerName: X-API-Key
    ipAllowlist:
      - 10.0.0.0/8
      - 192.168.0.0/16
    revocation:
      failOpen: false
```

### JWT (Most Common)

```bash
# Send JWT in Authorization header
curl -H "Authorization: Bearer eyJhbGc..." \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1
```

The API server:
1. Extracts the token
2. Verifies signature against JWKS (cached, refreshed every 5 minutes)
3. Validates expiration and audience
4. Maps claims to `Identity{actor_id, roles, attributes}`
5. Sets session context in Postgres

### OIDC (Browser-based)

```bash
# 1. User visits https://api.velocity.acme.com/auth/login?next=/orders
# 2. API redirects to IdP
# 3. User authenticates and consents
# 4. IdP redirects back to /auth/callback with authorization code
# 5. API exchanges code for token
# 6. Session cookie set; user redirected to /orders
```

### API Key

```bash
velocity api-key create --name prod --ttl 30d
# Output: vel_prod_abc123...

# Use it:
curl -H "X-API-Key: vel_prod_abc123..." \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1
```

API keys are:
- SHA256 hashed in the database (plaintext shown once at creation)
- IP-restricted (optional)
- Time-expiring (configurable TTL)
- Revocable instantly

### Composite (Try Multiple Strategies)

```yaml
type: composite
strategies:
  - name: jwt-internal
    weight: 1
  - name: jwt-partners
    weight: 2
  - name: api-key
    weight: 3
  # First successful authentication wins
```

## Access Control Layers (7 Layers)

### Layer 1: Route-Level RBAC

Does the actor have a role that allows this operation?

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
spec:
  access:
    roles:
      create: [procurement-writer]
      read: [procurement-reader, procurement-writer]
      update: [procurement-writer]
      delete: [procurement-admin]
```

If you lack a required role, you get 403 Forbidden before any data is touched.

### Layer 2: ABAC (Attribute-Based Access Control)

CEL expressions evaluated at request time:

```yaml
spec:
  access:
    abac:
      - operation: create
        condition: "actor.department in ['procurement', 'finance']"
        message: "Only procurement and finance can create POs"
      
      - operation: read
        condition: "actor.tenure_days > 30 || actor.is_manager"
        message: "New employees cannot read POs until 30-day review"
```

### Layer 3: Cross-Schema RBAC

Do you have read access to a schema you want to join?

Currently deferred to Phase 5 (when query joins are implemented). The gate will be: before adding a join, verify actor has read permission on the target schema.

### Layer 4: Row Filter

Scope rows by attribute:

```yaml
spec:
  access:
    rowFilter:
      - role: region-manager
        condition: "region = current_setting('app.current_region')"
        message: "Region managers see only their region"
      
      - role: store-manager
        condition: "store_id = ANY(current_setting('app.store_ids')::text[])"
```

A request sees only rows matching its filter.

### Layer 5: Field Filter (Read)

Hide sensitive fields on response:

```yaml
spec:
  access:
    fieldAccess:
      - field: cost_basis
        read: [finance-reader, finance-admin]
```

Actors without `cost_basis` read permission see `null` for that field.

### Layer 6: Field Filter (Write)

Reject payloads containing fields the actor can't write:

```yaml
spec:
  access:
    fieldAccess:
      - field: approved_by
        write: [procurement-admin]
```

If an actor without `approved_by` write permission includes it in PATCH, the request fails 403.

### Layer 7: Postgres RLS

The database itself enforces row-level security:

```sql
CREATE POLICY region_policy
ON purchase_order_v1
FOR ALL
USING (region = current_setting('app.current_region'))
```

Even if the app has a bug and returns all rows, RLS filters them.

## RoleBinding

Grant roles to actors:

```yaml
apiVersion: velocity.sh/v1
kind: RoleBinding
metadata:
  name: ravi.kumar-procurement
  namespace: acme-supply-chain-procurement
spec:
  actor: ravi.kumar
  roles: [procurement-reader, procurement-writer]
  expiryDate: "2027-12-31T23:59:59Z"
  scope:
    region: west
    store_ids: [10, 20, 30]
  attributes:
    department: procurement
    tenure_days: 365
```

Create via CLI:

```bash
velocity grant \
  --actor ravi.kumar \
  --roles procurement-reader,procurement-writer \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --scope region=west,store_ids=10:20:30 \
  --expires 2027-12-31
```

Revoke:

```bash
velocity revoke --actor ravi.kumar \
  --schema acme/supply-chain/procurement/purchase-order/v1
```

RoleBindings are stored in Postgres and cached in Redis for revocation checks.

## Claim Mapping

Transform JWT claims into Velocity identity attributes:

```yaml
spec:
  jwt:
    claimMapping:
      actorId: sub  # Required; identifies the actor
      
      roles:
        - path: realm_access.roles
          transform: identity
        
        - path: groups
          transform: prefix_strip
          prefix: "acme-"
      
      attributes:
        region:
          path: custom.region
          transform: identity
        
        store_ids:
          path: custom.stores
          transform: split
          separator: ","
        
        department:
          path: custom.dept
          transform: lookup  # Look up in Postgres table
          table: platform.department_mapping
          keyColumn: jwt_value
          valueColumn: dept_name
```

### Transforms

- **`identity`:** Use value as-is
- **`prefix_strip`:** Remove prefix (e.g., "acme-" → ["admin", "reader"])
- **`split`:** Split by separator (e.g., "a,b,c" → ["a", "b", "c"])
- **`uppercase` / `lowercase`:** Case transformation
- **`lookup`:** Join with a Postgres table (e.g., JWT email → internal user ID)
- **`regex_extract`:** Extract with regex groups
- **`static_append`:** Append a static value

## Fail-Mode Matrix (ADR-003)

When external dependencies fail, the system defaults to **deny** (fail-closed):

### Redis Unavailable (Revocation Check)

| Scenario | Default | Override |
|----------|---------|----------|
| Redis unreachable | 503 REVOCATION_UNAVAILABLE (deny) | `failOpen: true` (dangerous) |

### JWKS Endpoint Unavailable (Issuer)

| Scenario | Behavior |
|----------|----------|
| Cache hit (key in cache) | Allow (cryptographically verified) |
| Cache miss (key unknown) | 401 Invalid Token (deny) |
| Cache expired | 401 Invalid Token (deny) |

### Database Unavailable

| Scenario | Behavior |
|----------|----------|
| Postgres unreachable | 503 Service Unavailable (deny) |

## Audit of Auth Decisions

Every request logs its auth decision:

```sql
SELECT 
  actor, operation, strategy, 
  outcome, fail_mode, timestamp 
FROM platform.audit_log 
WHERE actor = 'ravi.kumar' 
ORDER BY timestamp DESC LIMIT 10;
```

Output:

```
actor         operation outcome              fail_mode timestamp
─────────────────────────────────────────────────────────────────────
ravi.kumar    CREATE    success              NONE      2026-05-19 14:32
ravi.kumar    READ      denied_rbac          NONE      2026-05-19 14:31
ravi.kumar    UPDATE    denied_cel_abac      NONE      2026-05-19 14:30
ravi.kumar    READ      success              REDIS_CACHED_ALLOWED  14:29
```

## Revoking Access

### Immediate Revocation (Actor)

Delete the RoleBinding:

```bash
velocity revoke --actor ravi.kumar \
  --schema acme/supply-chain/procurement/purchase-order/v1
```

The operator writes the actor to Redis's revocation set. Subsequent requests are denied within seconds.

### Token Expiration

Set short TTLs on tokens. A revoked actor's existing token remains valid until expiration. Shorter TTLs = faster revocation.

### API Key Revocation

```bash
velocity api-key revoke --name prod-secret
```

Immediate; no cache to clear.

## Best Practices

1. **Use short-lived tokens:** 15-60 minutes. Forces re-authentication and makes revocation faster.
2. **Scope JWT to audience:** `audience: acme-api` prevents token reuse across services.
3. **IP-restrict API keys:** If possible, use `ipAllowlist` to limit where keys can be used.
4. **Rotate API keys monthly:** Use `velocity api-key create` with a new name, revoke the old one.
5. **Use OIDC for browsers:** Cookies + session state is more secure than bearer tokens in browser storage.
6. **Use JWT for services:** Simpler, stateless, no session overhead.
7. **Test fail modes:** Kill Redis and verify requests are denied (not allowed).
8. **Monitor auth failures:** Alert on spike in 401 or 403 errors.

## Examples

### Create an AuthStrategy for JWT

```bash
kubectl apply -f - <<EOF
apiVersion: velocity.sh/v1
kind: AuthStrategy
metadata:
  name: jwt-acme
  namespace: platform
spec:
  type: jwt
  displayName: "Acme Internal JWT"
  jwt:
    issuer: https://auth.acme.com
    audience: acme-api
    jwksUrl: https://auth.acme.com/.well-known/jwks.json
    claimMapping:
      actorId: sub
      roles:
        - path: realm_access.roles
          transform: identity
      attributes:
        department:
          path: custom_claims.department
          transform: identity
    revocation:
      failOpen: false
EOF
```

### Test with curl

```bash
# Get a token (from your IdP)
TOKEN=$(curl -X POST https://auth.acme.com/token \
  -d "username=ravi.kumar&password=secret" | jq -r .access_token)

# Use it
curl -H "Authorization: Bearer $TOKEN" \
  https://api.velocity.acme.com/api/acme/supply-chain/procurement/purchase-order/v1
```

### Grant roles to a user

```bash
velocity grant \
  --actor ravi.kumar \
  --roles procurement-reader \
  --schema acme/supply-chain/procurement/purchase-order/v1 \
  --scope region=west \
  --expires 2026-12-31
```

### Create an API key for CI/CD

```bash
velocity api-key create \
  --name deploy-service \
  --ttl 90d

# Output: vel_deploy-service_xxxyyy...
# Use in deploy script: curl -H "X-API-Key: vel_deploy-service_xxxyyy..." ...
```
