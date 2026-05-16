# Velocity — Design (v2)

> Detailed CRD specs, API contracts, database conventions, and component interfaces.
> Incorporates ADRs 001–010 — see `decisions.md`.

---

## 1. CRD Specifications

### 1.1 Organisation

```yaml
apiVersion: velocity.sh/v1
kind: Organisation
metadata:
  name: acme
  namespace: platform
spec:
  displayName: "Acme Corp"
  tenancyMode: single        # single | multi-tenant (ADR-010)
  defaultAuthStrategy:
    name: jwt-internal
    namespace: platform
  defaultPolicies:
    audit:       strict
    retention:   7years
    timeMachine: true
  adminRoles: [platform-admin]
  resourceQuotas:
    maxApplications: 50
```

### 1.2 Application

```yaml
apiVersion: velocity.sh/v1
kind: Application
metadata:
  name: supply-chain
  namespace: acme-platform
  labels:
    velocity.sh/org: acme
spec:
  org:         acme
  displayName: "Supply Chain"
  owner:       ops-lead@acme.com
  team:        supply-chain-engineering
  authStrategy:
    name: jwt-internal
    namespace: platform
  resourceQuotas:
    maxSchemas:           50
    maxVersionsPerSchema: 3
    maxFieldsPerSchema:   100
    maxRecordsPerSchema:  500000000   # 500M ceiling per schema
    maxStorageGb:         1000
    requestsPerSecond:    5000
  databaseQuota:
    poolSize:             40           # PgBouncer connections allocated
    readReplicas:         2
```

### 1.3 Domain

```yaml
apiVersion: velocity.sh/v1
kind: Domain
metadata:
  name: procurement
  namespace: acme-supply-chain
  labels:
    velocity.sh/org: acme
    velocity.sh/app: supply-chain
spec:
  app: supply-chain
  displayName: "Procurement"
  access:
    defaultRole: procurement-reader
    adminRole:   procurement-admin
  databaseQuota:
    poolSize:        20      # connection allocation
    coldTablespace:  true    # use cold disk for archive schema
```

### 1.4 SchemaDefinition

```yaml
apiVersion: velocity.sh/v1
kind: SchemaDefinition
metadata:
  name: purchase-order
  namespace: acme-supply-chain-procurement
  labels:
    velocity.sh/org:    acme
    velocity.sh/app:    supply-chain
    velocity.sh/domain: procurement
  annotations:
    velocity.sh/owner: ops-lead@acme.com
spec:
  version: v2

  # ─── Partitioning (required for schemas expected to exceed 50M rows) ───
  partitioning:
    enabled:  true
    strategy: range        # range | list | hash
    field:    created_at
    interval: monthly      # monthly | quarterly | yearly
    retention: 7years      # auto-drop partitions older than this (from archive)

  # ─── Auth ──────────────────────────────────────────────
  auth:
    strategyRef:
      name:      jwt-internal
      namespace: platform
    overrides:
      - operations: [export, restore]
        strategyRef:
          name:      oidc-interactive
          namespace: platform

  # ─── Access ────────────────────────────────────────────
  access:
    roles:
      - role:       admin
        operations: [create, read, update, delete, restore, export, history]
      - role:       procurement-writer
        operations: [create, read, update]
      - role:       procurement-reader
        operations: [read]
    rowFilter:
      - role: store-operator
        filter:
          field: store_id
          op:    eq
          value: "{{identity.attributes.store_id}}"
    policies:
      - name: business-hours-update
        action:  update
        fields:  [unit_price]
        condition: "request.time.getHours() >= 8 && request.time.getHours() <= 20"
        message: "Price updates only during business hours"

  # ─── Fields ────────────────────────────────────────────
  fields:
    - name:     po_number
      type:     string
      required: true
      unique:   true        # generates partial unique index (WHERE deleted_at IS NULL)
      indexed:  true
      pattern:  "^PO-[0-9]{8}$"

    - name:       supplier_code
      type:       string
      required:   true
      filterable: true
      ref:
        org:     acme
        app:     supply-chain
        domain:  procurement
        object:  supplier
        version: v1
        key:     code

    - name:    unit_price
      type:    number
      min:     0
      max:     10000000
      sensitivity: financial
      access:
        read:  [admin, finance-team, procurement-writer]
        write: [admin, procurement-writer]
      masking:
        roles:
          procurement-reader:
            strategy: redact

  # ─── Validations ───────────────────────────────────────
  validations:
    - type:    compare
      left:    unit_price
      operator: lte
      right:   approved_budget
      message: "unit_price cannot exceed approved_budget"
    - type: cel
      rule: "self.status == 'cancelled' ? has(self.cancellation_reason) : true"
      message: "Cancellation reason required when cancelled"
      maxExecutionMs: 10       # ADR / CEL safety

  # ─── Search ────────────────────────────────────────────
  search:
    tier: 3
    cross_search:        true
    cross_search_weight: 8
    display:
      label:          "Purchase Order"
      title_field:    po_number
      subtitle_field: supplier_code
      url_template:   "/procurement/purchase-orders/{id}"

  # ─── Hooks ─────────────────────────────────────────────
  hooks:
    onCreate:
      - type: kafka
        # Per ADR-008 — topics are per-domain, not per-schema
        # Operator routes via header: velocity-event=created, velocity-schema=purchase-order
        retries: 3
    onUpdate:
      - type: http
        url:  "http://approval-svc/notify"
        retries: 2
        timeout: 5s

  # ─── Time machine ──────────────────────────────────────
  timeMachine:
    enabled: true
    storage:
      hot:
        backend:   postgres
        retention: 90days
      warm:
        backend:   s3
        format:    parquet
        retention: 5years
      cold:
        backend:   glacier
        retention: 7years

  # ─── Audit ─────────────────────────────────────────────
  audit:
    enabled: true
    reads:
      sensitiveFields: true
      bulkThreshold:   100
    writes:
      requireReason:    [delete, restore]
      requireTicketRef: [delete]
    regulations: [sebi]

  # ─── Archive ───────────────────────────────────────────
  archive:
    policyRef:
      name:      procurement-archive
      namespace: acme-supply-chain-procurement

  # ─── Observability ─────────────────────────────────────
  observability:
    slos:
      - operation:     create
        target_p99_ms: 200
        availability:  99.9
        window:        30d
      - operation:     read
        target_p99_ms: 100
        window:        7d

  # ─── Scaling ───────────────────────────────────────────
  scaling:
    min: 2
    max: 30
    triggers:
      - type: cpu
        threshold: 60
      - type: rps
        threshold: 500
```

### 1.5 AuthStrategy

```yaml
apiVersion: velocity.sh/v1
kind: AuthStrategy
metadata:
  name: jwt-internal
  namespace: platform
spec:
  type: jwt
  config:
    issuers:
      - issuer:   "https://auth.acme.com"
        jwks_url: "https://auth.acme.com/.well-known/jwks.json"
        audience: "velocity-api"
        claims:
          actor_id: "$.sub"
          email:    "$.email"
          roles:
            path: "$.roles"
            transform: static_append
            values: [authenticated]
          attributes:
            store_id:   "$.attributes.store_id"
            region:     "$.attributes.region"
    revocation:
      backend:        redis
      key:            "revoked_actors"
      failOpen:       false     # ADR-003 — default deny on Redis failure
      ttl:            86400     # seconds
    cel:
      maxExecutionMs: 10        # safety cap per evaluation
    ttl_max:    3600
    clock_skew: 30
```

### 1.6 ApiKey

```yaml
apiVersion: velocity.sh/v1
kind: ApiKey
metadata:
  name: erp-sync-key
  namespace: acme-supply-chain-procurement
spec:
  actor:      erp-sync-service
  actorType:  service
  scopes:
    - schema:     purchase-order
      version:    v2
      operations: [create, update]
      fields:
        write: [po_number, supplier_code, status, unit_price]
  ipAllowlist:
    - 10.0.0.0/8
  expiry:    2027-01-01
  status:
    secretRef: erp-sync-api-key-secret   # populated by operator on creation
    keyHash:   "sha256:abc123..."        # only the hash is stored
```

**Key format (per ADR — API key entropy):**
```
vel_{env}_{32 bytes base64}
Example: vel_prod_AB12cd34EF56gh78IJ90kl12MN34op56QR78st90UV12wx
Entropy: 256 bits
Storage: SHA256(key) — plaintext never stored
Display: shown ONCE via velocity create api-key output; not retrievable
Rotation: create new key with --copy-from <old>, switch consumer, revoke old
```

### 1.7 LogFilterPolicy and LogRoutingPolicy

(Unchanged from v1 — see original design.md for full spec.)

### 1.8 ArchivePolicy

(Unchanged from v1 — see original design.md for full spec.)

---

## 2. API Design

### 2.1 Standard CRUD with idempotency

```
POST /api/{org}/{app}/{domain}/{object}/{version}

Headers:
  Authorization:    Bearer <token>
  Idempotency-Key:  <client-generated UUID>    # optional but recommended
  X-Request-ID:     <uuid>
  X-Reason:         <string>                   # required if schema.audit configured
  X-Ticket-Ref:     <string>                   # required if schema.audit configured

Response on idempotency replay:
  Same response as original request, with header X-Idempotent-Replay: true
  Stored for 24h in platform.idempotency_keys
```

### 2.2 Pagination (per ADR-009)

```
# Cursor-based (default, required for limit + offset > 1000)
GET /api/.../v2?limit=50&cursor=<opaque>

Response:
{
  "data": [...],
  "pagination": {
    "next_cursor": "eyJv...",      # HMAC-signed
    "has_more":    true
  }
}

# Offset-based (small queries only)
GET /api/.../v2?limit=50&offset=100

Response:
{
  "data": [...],
  "pagination": {
    "total":      1234,
    "page":       2,
    "page_size":  50,
    "has_more":   true
  }
}

# Request with offset + limit > 1000:
{
  "status":  400,
  "code":    "PAGINATION_LIMIT_EXCEEDED",
  "message": "Use cursor-based pagination for result sets > 1000"
}
```

### 2.3 Query DSL with cross-schema RBAC

```json
POST /api/{...}/v2/query

{
  "where": { ... },
  "include": ["supplier"],          # join — requires read access on supplier schema
  "order_by": [{ "field": "created_at", "dir": "desc" }],
  "limit": 50,
  "cursor": null
}
```

**Cross-schema RBAC** (per ADR — was missing):
```rust
for relation in &dsl.include {
    let target = schema.relation(relation)?.resolve(&registry)?;
    if !identity.can_read(&target) {
        return Err(ApiError::Forbidden(format!(
            "Cannot include {}: no read access on {}/{}/{}/{}",
            relation, target.org, target.app, target.domain, target.kind
        )));
    }
}
```

### 2.4 Time Machine endpoints

```
GET  /{id}/history                         # paginated history
GET  /{id}/history?at=<ISO8601>            # state at point in time
GET  /{id}/diff?from=<T1>&to=<T2>          # field-level diff
POST /{id}/restore                          # apply old state as new event
GET  /{id}/replay                          # SSE stream

# Tier routing (transparent to caller):
#   age <  90 days  → hot tier (Postgres)         <50ms
#   age <  5 years  → warm tier (S3 Parquet)      2-10s
#   age >= 5 years  → cold tier (Glacier)         async, 5-12hr

# Cold tier response:
{
  "status":   202,
  "message":  "Cold tier retrieval scheduled",
  "job_id":   "retrieve-abc123",
  "estimated_completion": "2026-05-16T15:00:00Z",
  "callback_url": "/api/jobs/retrieve-abc123"
}

# Restore no-op detection (per review):
# If target state == current state, return 409:
{
  "status":  409,
  "code":    "RESTORE_NO_OP",
  "message": "Target state matches current state; no restore performed"
}
```

### 2.5 Error shape

```json
{
  "status":     422,
  "code":       "VALIDATION_ERROR",
  "message":    "Human-readable summary",
  "request_id": "req-abc123",
  "errors": [
    {
      "field":   "unit_price",
      "rule":    "compare",
      "message": "unit_price must not exceed approved_budget"
    }
  ]
}
```

Error codes:
```
AUTH_REQUIRED        VALIDATION_ERROR         CONFLICT (optimistic lock)
FORBIDDEN            FIELD_FORBIDDEN          PAGINATION_LIMIT_EXCEEDED
NOT_FOUND            QUOTA_EXCEEDED           RESTORE_NO_OP
SCHEMA_DEPRECATED    RATE_LIMITED             COLD_TIER_RETRIEVAL_REQUIRED
IDEMPOTENCY_CONFLICT (same key, different payload)
```

---

## 3. Database Conventions

### 3.1 Naming

```
Postgres schema:        {org}_{app}_{domain}             snake_case
Postgres schema (cold): {org}_{app}_{domain}_archive
Table name:             {object}_{version}
History table:          {object}_{version}_history
Outbox table:           {object}_{version}_outbox
Connection role:        velocity_api (single, non-superuser, NOBYPASSRLS)
Per-domain roles:       {org}_{app}_{domain}_{reader|writer|admin}
```

### 3.2 Mandatory columns

```sql
id          UUID        NOT NULL DEFAULT gen_random_uuid() PRIMARY KEY
created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
deleted_at  TIMESTAMPTZ                                   -- soft delete
version     INTEGER     NOT NULL DEFAULT 1                -- optimistic lock
created_by  TEXT        NOT NULL                          -- SET LOCAL app.current_user
updated_by  TEXT        NOT NULL
archived_at TIMESTAMPTZ
archive_ref TEXT
```

### 3.3 Generated unique constraints — always partial

```sql
-- Correct: deleted records don't block re-use of unique values
CREATE UNIQUE INDEX idx_{table}_{field}_active
ON {table} ({field})
WHERE deleted_at IS NULL;
```

### 3.4 Outbox table (Tier-3 search schemas only)

```sql
CREATE TABLE {schema}.{table}_outbox (
  id           BIGSERIAL PRIMARY KEY,
  op           TEXT NOT NULL,         -- INSERT, UPDATE, DELETE
  entity_id    UUID NOT NULL,
  payload      JSONB,                  -- row content; NULL for DELETE
  occurred_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  published_at TIMESTAMPTZ
);

CREATE INDEX ON {schema}.{table}_outbox (published_at)
  WHERE published_at IS NULL;

-- Cleanup job: DELETE WHERE published_at < now() - '24 hours'::interval
```

### 3.5 Trigger function — outbox write

The same trigger that writes history also writes outbox (single trigger, single transaction):

```sql
CREATE FUNCTION {schema}.{table}_audit_and_outbox()
RETURNS TRIGGER AS $$
BEGIN
  -- History
  INSERT INTO {schema}.{table}_history (...)
    VALUES (...);
  
  -- Outbox (only for tier-3 schemas)
  INSERT INTO {schema}.{table}_outbox (op, entity_id, payload)
    VALUES (TG_OP, COALESCE(NEW.id, OLD.id),
            CASE WHEN TG_OP = 'DELETE' THEN NULL ELSE to_jsonb(NEW) END);
  
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;
```

### 3.6 Audit log — DB-level chain construction (ADR-005)

```sql
CREATE TABLE platform.audit_chain_state (
  id        INTEGER PRIMARY KEY DEFAULT 1,
  last_hash TEXT,
  CONSTRAINT singleton CHECK (id = 1)
);
INSERT INTO platform.audit_chain_state (id, last_hash) VALUES (1, NULL);

CREATE FUNCTION platform.audit_insert(
  p_actor      TEXT,
  p_action     TEXT,
  p_outcome    TEXT,
  p_schema_org TEXT,
  p_entity_id  UUID,
  p_payload    JSONB
) RETURNS UUID AS $$
DECLARE
  v_id         UUID := gen_random_uuid();
  v_prev_hash  TEXT;
  v_new_hash   TEXT;
BEGIN
  -- Serialize on the singleton row
  UPDATE platform.audit_chain_state
    SET last_hash = last_hash    -- no-op; forces lock acquisition
    WHERE id = 1
    RETURNING last_hash INTO v_prev_hash;

  v_new_hash := encode(digest(
    v_id::text || now()::text || p_actor || p_action ||
    p_entity_id::text || coalesce(v_prev_hash, ''),
    'sha256'
  ), 'hex');

  INSERT INTO platform.audit_log
    (id, occurred_at, actor, action, outcome, schema_org,
     entity_id, payload, prev_hash, hash)
    VALUES
    (v_id, now(), p_actor, p_action, p_outcome, p_schema_org,
     p_entity_id, p_payload, v_prev_hash, v_new_hash);

  UPDATE platform.audit_chain_state
    SET last_hash = v_new_hash
    WHERE id = 1;

  RETURN v_id;
END;
$$ LANGUAGE plpgsql;

-- Application always calls the function, never INSERTs directly
GRANT EXECUTE ON FUNCTION platform.audit_insert TO velocity_api;
REVOKE INSERT, UPDATE, DELETE ON platform.audit_log FROM PUBLIC;
```

Verification:
```sql
-- Walk the chain, recompute each hash, detect tamper
WITH RECURSIVE chain AS (
  SELECT id, occurred_at, prev_hash, hash, actor, action, entity_id,
         encode(digest(
           id::text || occurred_at::text || actor || action ||
           entity_id::text || coalesce(prev_hash, ''),
           'sha256'
         ), 'hex') AS computed_hash
  FROM platform.audit_log
)
SELECT id, occurred_at, hash, computed_hash
FROM chain
WHERE hash != computed_hash;
```

### 3.7 Field type mapping

```
velocity type    Postgres type            Notes
─────────────────────────────────────────────────────────
string           TEXT
string(n)        VARCHAR(n)
number           NUMERIC(19,4)            financial precision default
integer          BIGINT
boolean          BOOLEAN
date             DATE
datetime         TIMESTAMPTZ
uuid             UUID
json             JSONB
enum             TEXT + CHECK
ref (same domain)  TEXT + FOREIGN KEY
ref (cross domain) TEXT + application-layer FK
```

### 3.8 Auto-generated indexes

```sql
-- indexed: true
CREATE INDEX idx_{table}_{field} ON {table} ({field});

-- filterable: true on date/timestamp
CREATE INDEX idx_{table}_{field} ON {table} ({field});

-- soft delete (all tables)
CREATE INDEX idx_{table}_active ON {table} (deleted_at) WHERE deleted_at IS NULL;

-- FTS (searchable fields)
CREATE INDEX idx_{table}_fts ON {table} USING GIN (search_vector);

-- JSONB
CREATE INDEX idx_{table}_{field}_gin ON {table} USING GIN ({field});

-- Large tables: build CONCURRENTLY
-- threshold: > 1M rows
```

### 3.9 Partitioned tables (for schemas with `partitioning.enabled: true`)

```sql
-- Range partition (most common — by created_at)
CREATE TABLE {schema}.{table} (
  ...,
  CONSTRAINT {table}_pkey PRIMARY KEY (id, created_at)   -- partition key in PK
) PARTITION BY RANGE (created_at);

-- Operator creates monthly partitions
CREATE TABLE {table}_2026_01 PARTITION OF {table}
  FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
CREATE TABLE {table}_2026_02 PARTITION OF {table}
  FOR VALUES FROM ('2026-02-01') TO ('2026-03-01');
-- ...

-- Operator runs nightly to:
--   - create next 3 months of partitions
--   - detach + drop partitions older than retention
```

---

## 4. SchemaRegistry Interface (ADR-001 & ADR-006)

```rust
// Each API server replica owns its own registry, fed by kube informer
struct SchemaRegistry {
    inner: arc_swap::ArcSwap<RegistryInner>,
}

struct RegistryInner {
    by_path:   HashMap<SchemaPath, Arc<ResolvedSchema>>,
    by_org:    HashMap<String, Vec<SchemaPath>>,
    by_app:    HashMap<(String, String), Vec<SchemaPath>>,
    by_domain: HashMap<(String, String, String), Vec<SchemaPath>>,
    by_object: HashMap<ObjectKey, Vec<VersionedEntry>>,
    ref_graph: RefGraph,
    routers:   HashMap<ObjectKey, VersionRouter>,
}

impl SchemaRegistry {
    // Read path — lock-free, sub-microsecond
    pub fn resolve(&self, path: &SchemaPath) -> Option<Arc<ResolvedSchema>> {
        self.inner.load().by_path.get(path).cloned()
    }

    pub fn resolve_version(&self, key: &ObjectKey, version: Option<&str>)
        -> Option<Arc<ResolvedSchema>>
    {
        let inner = self.inner.load();
        let versions = inner.by_object.get(key)?;
        match version {
            Some(v) => versions.iter().find(|e| e.version == v).map(|e| e.schema.clone()),
            None    => versions.iter()
                       .filter(|e| e.lifecycle == Lifecycle::Stable)
                       .max_by_key(|e| &e.version)
                       .map(|e| e.schema.clone()),
        }
    }

    pub fn schemas_for_cross_search(&self, scope: &SearchScope)
        -> Vec<Arc<ResolvedSchema>>
    {
        let inner = self.inner.load();
        match scope {
            SearchScope::Org(o)         => inner.by_org.get(o),
            SearchScope::App(o, a)      => inner.by_app.get(&(o.clone(), a.clone())),
            SearchScope::Domain(o,a,d)  => inner.by_domain.get(&(o.clone(), a.clone(), d.clone())),
        }
        .map(|paths| paths.iter()
                       .filter_map(|p| inner.by_path.get(p).cloned())
                       .filter(|s| s.search.cross_search)
                       .collect())
        .unwrap_or_default()
    }

    // Write path — clone and swap; called by informer
    pub fn upsert(&self, schema: ResolvedSchema) {
        self.inner.rcu(|current| {
            let mut new = (**current).clone();
            new.upsert(schema.clone());
            new
        });
    }

    pub fn remove(&self, path: &SchemaPath) {
        self.inner.rcu(|current| {
            let mut new = (**current).clone();
            new.remove(path);
            new
        });
    }

    // Used during informer restart — full state replacement
    pub fn replace_all(&self, schemas: Vec<ResolvedSchema>) {
        let mut inner = RegistryInner::default();
        for s in schemas {
            inner.upsert(s);
        }
        self.inner.store(Arc::new(inner));
    }
}
```

---

## 5. QueryBuilder Interface

```rust
pub struct QueryBuilder<'a> {
    schema:     &'a ResolvedSchema,
    identity:   &'a Identity,
    registry:   &'a SchemaRegistry,
    conditions: Vec<Condition>,
    order:      Vec<OrderClause>,
    select:     Vec<FieldRef>,
    joins:      Vec<JoinClause>,
    aggregates: Option<AggregateSpec>,
    pagination: Pagination,        // Cursor or Offset (ADR-009)
}

impl<'a> QueryBuilder<'a> {
    pub fn from_dsl(
        dsl: &QueryDsl,
        schema: &'a ResolvedSchema,
        identity: &'a Identity,
        registry: &'a SchemaRegistry,
    ) -> Result<Self, QueryError> {
        let mut qb = Self::new(schema, identity, registry);

        // Cross-schema RBAC for joins (ADR / review fix)
        if let Some(includes) = &dsl.include {
            for relation_name in includes {
                let relation = schema.relation(relation_name)
                    .ok_or(QueryError::UnknownRelation(relation_name.clone()))?;
                let target = registry.resolve(&relation.target_path)
                    .ok_or(QueryError::TargetSchemaNotFound)?;
                if !identity.can_read(&target) {
                    return Err(QueryError::CrossSchemaAccessDenied {
                        relation: relation_name.clone(),
                        target:   target.kind.clone(),
                    });
                }
                qb.joins.push(JoinClause::from(relation, target));
            }
        }

        qb.apply_where(&dsl.where_)?;
        qb.apply_order(&dsl.order_by)?;
        qb.apply_select(&dsl.select)?;
        qb.apply_aggregate(&dsl.aggregate)?;
        qb.apply_pagination(&dsl.pagination)?;

        Ok(qb)
    }

    // Always called — cannot be omitted
    fn apply_rls(&mut self) {
        if let Some(filter) = self.schema.row_filter_for(self.identity) {
            self.conditions.push(filter.resolved(self.identity));
        }
    }

    // Always called — cannot be omitted
    fn apply_soft_delete(&mut self) {
        self.conditions.push(Condition::is_null("deleted_at"));
    }

    pub fn build(mut self) -> (String, Vec<PgValue>) {
        self.apply_rls();              // hard-coded — never optional
        self.apply_soft_delete();      // hard-coded — never optional
        self.build_internal()
    }
}
```

**Invariants enforced at build time:**

1. Every field name validated against `schema.fields`
2. Only `filterable: true` fields in WHERE
3. Only `sortable: true` fields in ORDER BY
4. Only declared relations in JOIN
5. Joined schemas: actor has read access (ADR-explicit)
6. RLS filter present (cannot be removed)
7. Soft delete filter present (cannot be removed)
8. All values $N parameters, never string-interpolated
9. Max join depth: 2
10. Limit ≤ schema.maxResults (default 1000)
11. Offset + limit ≤ 1000 (otherwise require cursor)

---

## 6. CEL Safety (review fix S2)

```rust
async fn evaluate_cel(
    program: &Program,
    input: &Value,
    max_ms: u64,
) -> Result<bool, ValidationError> {
    let mut ctx = Context::default();
    ctx.add_variable("self", input)?;

    let result = tokio::time::timeout(
        Duration::from_millis(max_ms),
        program.execute(&ctx),
    )
    .await
    .map_err(|_| ValidationError::CelTimeout)?;

    Ok(result?.to_bool())
}
```

**Constraints applied at validating webhook (operator side):**

```rust
fn validate_cel_expression(expr: &str) -> Result<(), WebhookError> {
    // Reject if expression > 10KB
    if expr.len() > 10_240 {
        return Err(WebhookError::CelTooLarge);
    }
    
    // Reject if expression nests > 10 levels deep
    if cel_nesting_depth(expr) > 10 {
        return Err(WebhookError::CelTooNested);
    }
    
    // Compile to verify syntax — reject malformed
    Program::compile(expr)
        .map_err(|e| WebhookError::CelSyntax(e.to_string()))?;
    
    // Reject expressions using disallowed functions
    let ast = parse_cel_ast(expr)?;
    if ast.uses_function("matches") {
        // matches() can be catastrophic with adversarial regex; require explicit allowlist
        return Err(WebhookError::CelMatchesNotAllowed);
    }
    
    Ok(())
}
```

---

## 7. Event Log Schema (hot tier — partitioned)

```sql
CREATE TABLE platform.event_log (
  id          UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
  occurred_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
  sequence    BIGSERIAL    NOT NULL,

  schema_org     TEXT NOT NULL,
  schema_app     TEXT NOT NULL,
  schema_domain  TEXT NOT NULL,
  schema_object  TEXT NOT NULL,
  schema_version TEXT NOT NULL,

  entity_id   UUID    NOT NULL,
  entity_ver  INTEGER NOT NULL,
  operation   TEXT    NOT NULL,
  source      TEXT    NOT NULL,

  actor       TEXT NOT NULL,
  actor_type  TEXT NOT NULL,
  request_id  TEXT,
  reason      TEXT,
  ticket_ref  TEXT,

  state       JSONB NOT NULL,
  patch       JSONB
) PARTITION BY RANGE (occurred_at);

-- Monthly partitions
CREATE TABLE event_log_2026_01 PARTITION OF platform.event_log
  FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
-- ...

CREATE INDEX ON platform.event_log (schema_object, entity_id, sequence);
CREATE INDEX ON platform.event_log (schema_object, occurred_at);
CREATE INDEX ON platform.event_log (actor, occurred_at);
```

**Warm tier export** (operator nightly job):
```
1. Identify oldest hot partition no longer in retention
2. Export to Parquet on S3: s3://velocity-events/{schema}/{year}/{month}/
3. Verify Parquet readable via DuckDB
4. ALTER TABLE event_log DETACH PARTITION event_log_2026_01
5. DROP TABLE event_log_2026_01
```

---

## 8. Audit Log Schema (ADR-005)

```sql
CREATE TABLE platform.audit_log (
  id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  occurred_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

  category   TEXT NOT NULL,
  action     TEXT NOT NULL,
  outcome    TEXT NOT NULL,

  actor_id   TEXT NOT NULL,
  actor_type TEXT NOT NULL,
  actor_ip   INET,
  session_id TEXT,

  schema_org     TEXT,
  schema_app     TEXT,
  schema_domain  TEXT,
  schema_object  TEXT,
  schema_version TEXT,
  entity_id      UUID,

  request_id      TEXT NOT NULL,
  fields_accessed TEXT[],
  fields_changed  TEXT[],

  reason     TEXT,
  ticket_ref TEXT,

  fail_mode  TEXT,                   -- which fail-mode applied (from ADR-003)
  error_code TEXT,

  data_class  TEXT[],
  regulations TEXT[],

  prev_hash TEXT,
  hash      TEXT NOT NULL
);

-- Immutable via RLS
ALTER TABLE platform.audit_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY audit_no_modify ON platform.audit_log
  AS RESTRICTIVE FOR UPDATE USING (false);
CREATE POLICY audit_no_delete ON platform.audit_log
  AS RESTRICTIVE FOR DELETE USING (false);

GRANT EXECUTE ON FUNCTION platform.audit_insert TO velocity_api;
GRANT SELECT ON platform.audit_log TO velocity_audit_reader;
REVOKE INSERT, UPDATE, DELETE ON platform.audit_log FROM PUBLIC;
```

---

## 9. Idempotency Store

```sql
CREATE TABLE platform.idempotency_keys (
  key            TEXT PRIMARY KEY,
  actor_id       TEXT NOT NULL,
  request_hash   TEXT NOT NULL,         -- SHA256 of method + path + body
  response_body  JSONB,
  response_code  INTEGER,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ON platform.idempotency_keys (created_at);
-- Cleanup job: DELETE WHERE created_at < now() - '24 hours'::interval
```

API server flow:
```
1. Receive request with Idempotency-Key header
2. Lookup key
3. If found and request_hash matches → return stored response
4. If found and request_hash differs → 409 IDEMPOTENCY_CONFLICT
5. If not found → process request, store result before responding
```

---

## 10. Cursor Format

```json
{
  "v":    "1",                      // version
  "o":    "created_at",             // order field
  "lv":   "2026-05-16T10:00:00Z",   // last value
  "lid":  "uuid-last-record",       // last id (for stable tie-break)
  "dir":  "desc"                    // direction
}
```

Base64-encoded and HMAC-signed with a server secret. Tampering invalidates.

---

## 11. Log Entry Shape (unchanged from v1)

Structured JSON, one line per entry. Includes `velocity.{org,app,domain}` labels added by `LogProcessor` enrichment phase.
