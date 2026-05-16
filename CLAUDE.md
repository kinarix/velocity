# CLAUDE.md — Velocity Platform (v2)

> Implementation guide for Claude when building Velocity.
> Read this before writing any code. Every section is load-bearing.
>
> **v2 changes:** Incorporates ADRs 001–010. Non-superuser DB role enforced from day one. arc_swap for registry. CDC via outbox. Failure mode matrix. CEL safety constraints.

---

## What is Velocity

Velocity is a schema-driven, Kubernetes-native backend platform built in Rust. Developers apply a `SchemaDefinition` CRD; the platform provisions a Postgres table, REST API, validation, search, auth, audit, time machine, and observability — automatically.

The platform is org-agnostic. Org is the first segment of `org/app/domain/object/version`, configured at deploy time.

---

## Required Reading Before Implementation

1. `docs/architecture.md` — system overview, components, data flow
2. `docs/design.md` — CRD specs, API contracts, database conventions
3. `docs/decisions.md` — ADRs 001–010 (foundational decisions, do not deviate without an ADR update)
4. `docs/operations.md` — backup, restore, failover, runbooks
5. `docs/phases.md` — phased delivery plan

---

## Repository Structure

```
velocity/
├── crates/
│   ├── velocity-types/         # CRD structs, shared types
│   ├── velocity-operator/      # kube-rs operators (5 reconcilers)
│   ├── velocity-api/           # Axum API server
│   ├── velocity-log-processor/ # Log enrichment, filter, route
│   ├── velocity-log-collector/ # DaemonSet log shipper
│   ├── velocity-cli/           # velocity binary
│   ├── velocity-webhook/       # ValidatingWebhook server
│   └── velocity-archive-worker/# Archive batch worker
├── charts/                     # Helm charts
├── crds/                       # Generated CRD YAML (do not edit manually)
├── migrations/                 # platform.* SQL
├── portal/                     # React admin portal
├── tests/                      # Integration tests
├── runbooks/                   # Operational runbooks (per operations.md)
└── docs/
```

---

## Tech Stack — Non-negotiable Choices

| Concern | Crate | Rationale |
|---------|-------|-----------|
| API framework | `axum` | Type-safe, tower middleware |
| Operator | `kube`, `kube-runtime` | Informers, leader election |
| CRD generation | `kube::CustomResource` + `schemars` | Rust struct → CRD manifest |
| Concurrency primitive | `arc_swap` (ADR-006) | Lock-free reads for SchemaRegistry |
| DB driver | `sqlx` | Compile-time queries |
| DB role | non-superuser `velocity_api` (ADR-007) | Makes RLS an actual backstop |
| Schema derivation | `schemars` | JSON Schema from Rust types |
| Expression language | `cel-interpreter` + tokio timeout | Safe CEL with bounded execution |
| HTTP client | `reqwest` | Async, TLS |
| CLI | `clap` v4 | Derive macros |
| Metrics | `prometheus` or `opentelemetry-prometheus` | Standard format |
| Tracing | `tracing` + `opentelemetry` | OTel SDK |
| Logging | `tracing-subscriber` JSON formatter | Stdout, one line per event |
| Runtime | `tokio` | Standard async |
| JWT | `jsonwebtoken` | JWKS verification |
| JSONPath | `jsonpath-rust` | Claim mapping |
| JSON Patch | `json-patch` | Time machine diffs |
| Hashing | `sha2` | Audit chain |
| DuckDB | `duckdb` (Rust bindings) | Warm tier Parquet queries |
| Parquet writer | `arrow` + `parquet` | Warm tier export |

---

## CRD Conventions

### Group and version

All CRDs use `velocity.sh/v1`:

```rust
#[derive(CustomResource, JsonSchema, Serialize, Deserialize, Clone, Debug)]
#[kube(
    group     = "velocity.sh",
    version   = "v1",
    kind      = "SchemaDefinition",
    namespaced,
    status    = "SchemaDefinitionStatus",
    shortname = "sd",
)]
pub struct SchemaDefinitionSpec { /* ... */ }
```

### Labels — always set by operator

```yaml
labels:
  velocity.sh/org:     acme
  velocity.sh/app:     supply-chain
  velocity.sh/domain:  procurement
  velocity.sh/version: v2
```

The validating webhook rejects mismatches between namespace and `{org}-{app}-{domain}`.

### Generating CRD manifests

```bash
cargo run --bin generate-crds
# outputs crds/*.yaml
```

Never edit `crds/*.yaml` manually. They are always generated.

---

## Code Structure

- Ensure 100% unit test coverage
- Update unit tests with every iteration

## SchemaRegistry Implementation (ADR-001, ADR-006)

**The registry is fed by a kube informer running inside each API server replica.** Not by RPC from the operator. Not by Redis pub/sub. Direct informer on etcd.

```rust
struct SchemaRegistry {
    inner: arc_swap::ArcSwap<RegistryInner>,
    ready: tokio::sync::watch::Sender<bool>,
}

async fn run_informer(
    registry: Arc<SchemaRegistry>,
    kube: kube::Client,
) -> ! {
    let api: kube::Api<SchemaDefinition> = kube::Api::all(kube);
    let watcher = kube::runtime::watcher(api, watcher::Config::default());
    pin_mut!(watcher);
    
    while let Some(event) = watcher.try_next().await
        .expect("informer disconnected")
    {
        match event {
            watcher::Event::Applied(crd) => {
                let resolved = resolve_schema(&crd).await
                    .unwrap_or_else(|e| {
                        tracing::error!(error = %e, "failed to resolve schema");
                        return;
                    });
                registry.upsert(resolved);
            }
            watcher::Event::Deleted(crd) => {
                registry.remove(&crd.path());
            }
            watcher::Event::Restarted(crds) => {
                // Initial sync or reconnect after disruption
                let resolved: Vec<_> = crds.into_iter()
                    .filter_map(|c| resolve_schema(&c).ok())
                    .collect();
                registry.replace_all(resolved);
                registry.ready.send(true).ok();
            }
        }
    }
    
    panic!("informer terminated");
}
```

**Readiness gate:** The Axum server's `/readyz` returns 200 only when the registry has received its first `Restarted` event. Until then, k8s service excludes this pod.

**Reads are lock-free** — `inner.load()` is an atomic pointer load. Use it everywhere on the hot path.

---

## Operator Patterns

### Reconciler structure

```rust
async fn reconcile(
    obj: Arc<SchemaDefinition>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    // 1. Resolve effective policy from PolicyTree
    let policy = ctx.policy_tree.effective_policy(&obj).await?;
    
    // 2. Skip if no change (policy hash unchanged + spec unchanged)
    let hash = compute_hash(&obj.spec, &policy);
    if let Some(prev) = ctx.last_reconciled_hash.get(&obj.metadata.uid) {
        if *prev == hash {
            return Ok(Action::requeue(Duration::from_secs(300)));
        }
    }
    
    // 3. Provision Postgres (idempotent — ADR-007 role context)
    ctx.provisioner.sync_table(&obj, &policy).await?;
    ctx.provisioner.sync_history_table(&obj).await?;
    if obj.spec.search.tier == 3 {
        ctx.provisioner.sync_outbox_table(&obj).await?;   // ADR-002
    }
    
    // 4. Generate HPA + KEDA
    ctx.scaler.sync(&obj, &policy).await?;
    
    // 5. Generate Grafana dashboard
    ctx.dashboards.upsert(&obj).await?;
    
    // 6. Generate Prometheus alerting rules
    ctx.alerts.sync_slo_rules(&obj).await?;
    
    // 7. Search index management (with blue-green if fields changed)
    if obj.spec.search.tier == 3 {
        ctx.search.sync_collection(&obj).await?;
    }
    
    // 8. Update status
    update_status(&obj, SchemaDefinitionStatus::Ready, &ctx.client).await?;
    
    // 9. Cache hash
    ctx.last_reconciled_hash.insert(obj.metadata.uid.clone(), hash);
    
    // 10. Requeue for drift detection
    Ok(Action::requeue(Duration::from_secs(300)))
}
```

### Idempotency requirement

Every reconciler must be **fully idempotent**. Use `CREATE IF NOT EXISTS`, check before applying. Running twice produces the same result.

### Blocking breaking changes

```rust
enum MigrationOp {
    AddColumn(ColumnDef),          // safe — auto-apply
    AddConstraint(ConstraintDef),  // safe if existing data passes — auto-apply
    AddIndex(IndexDef),            // safe — auto-apply CONCURRENTLY
    DropColumn(String),            // BREAKING — block
    ChangeType(String, TypeChange),// BREAKING — block
    RenameColumn(String, String),  // BREAKING — block
}

// Allow breaking ops only with:
// metadata.annotations[velocity.sh/breaking-change] == "approved"
```

### Reconcile storm prevention

When parent CRDs change (Org/App/Domain policies), child reconciles are queued with jitter:

```rust
async fn cascade_to_children(
    parent: &Organisation,
    affected: Vec<SchemaDefinition>,
    ctx: &Context,
) {
    let new_hash = hash(&parent.spec.policies);
    let to_reconcile: Vec<_> = affected
        .into_iter()
        .filter(|s| s.status.policy_hash != new_hash)
        .collect();
    
    for (i, schema) in to_reconcile.iter().enumerate() {
        let delay = Duration::from_millis(50 * (i / 10) as u64);  // 10 concurrent
        ctx.work_queue.add_delayed(schema.clone(), delay);
    }
}
```

### Validating webhook checks

Before allowing a SchemaDefinition apply:

1. Namespace matches `{org}-{app}-{domain}`
2. Referenced `AuthStrategy` exists
3. Referenced `ArchivePolicy` and `LogFilterPolicy` exist if specified
4. Cross-domain `ref` fields resolve to stable schemas
5. App quota not exceeded
6. Version is compatible with previous version (no field removal without annotation)
7. CEL expressions: syntactically valid, < 10KB, depth ≤ 10, no `matches()` with unbounded patterns
8. Per ADR-010: in multi-tenant mode, no cross-org `ref`s

---

## API Server Patterns

### Dynamic routing

Routes are built from `SchemaRegistry`. Use Axum's nested routers — one route prefix per schema path. Hot-reload via `tower::ServiceExt::reset_service()` or rebuild router on registry change.

### Request context

```rust
async fn create(
    State(state):       State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(audit):    Extension<AuditContext>,
    Path(path):          Path<SchemaPath>,
    Json(payload):       Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let schema = state.registry.resolve(&path)
        .ok_or(ApiError::SchemaNotFound)?;
    // ...
}
```

### Postgres session context (ADR-007)

Every transaction must set these:

```rust
async fn with_session_context<F, T>(
    pool: &PgPool,
    domain_role: &str,
    identity: &Identity,
    f: F,
) -> Result<T, sqlx::Error>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> BoxFuture<'c, Result<T, sqlx::Error>>,
{
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    
    // ADR-007: select per-domain role
    sqlx::query(&format!("SET LOCAL ROLE {domain_role}"))
        .execute(&mut *tx).await?;
    
    // Set actor context for RLS and audit
    sqlx::query("SET LOCAL app.current_user = $1")
        .bind(&identity.actor_id)
        .execute(&mut *tx).await?;
    
    if let Some(store_id) = identity.attributes.get("store_id") {
        sqlx::query("SET LOCAL app.current_store_id = $1")
            .bind(store_id)
            .execute(&mut *tx).await?;
    }
    
    let result = f(&mut tx).await?;
    tx.commit().await?;
    Ok(result)
}
```

### Authentication fail mode matrix (ADR-003)

```rust
async fn check_revocation(
    actor_id: &str,
    redis: &RedisClient,
    strategy: &AuthStrategy,
) -> Result<(), AuthError> {
    match redis.sismember("revoked_actors", actor_id).await {
        Ok(true)  => Err(AuthError::Revoked),
        Ok(false) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "redis revocation check failed");
            metrics::auth_dependency_failure.inc();
            
            if strategy.config.revocation.fail_open {
                // Per ADR-003 — explicit opt-out
                Ok(())
            } else {
                // Default — deny
                Err(AuthError::RevocationCheckUnavailable)
            }
        }
    }
}
```

Record the fail-mode applied in the audit log entry. Always.

### SQL safety

**Never interpolate user input into SQL strings. Use parameterized queries.**

```rust
// CORRECT — table name from registry (validated CRD)
let table = schema.postgres_table_name();
sqlx::query(&format!("SELECT * FROM {} WHERE id = $1", table))
    .bind(id)
    .fetch_one(&pool)
    .await?;

// WRONG — never use user input in SQL strings
let query = format!("SELECT * FROM {} WHERE id = '{}'", table, user_id);  // NO
```

Table names come from `SchemaRegistry.resolve()`, never from path parameters directly.

### QueryBuilder invariants

Enforced at `build()` time:

1. Every field name validated against `schema.fields`
2. Only `filterable: true` fields in WHERE
3. Only `sortable: true` fields in ORDER BY
4. Only declared relations in JOIN
5. Cross-schema RBAC checked before adding join (ADR / review fix)
6. RLS filter present (cannot be removed by caller)
7. Soft delete filter present
8. All values $N parameters
9. Limit + offset ≤ 1000, else require cursor (ADR-009)
10. Max join depth: 2

If any invariant fails: return `Err`, do not produce SQL.

### Idempotency

```rust
async fn handle_with_idempotency<F, T>(
    pool: &PgPool,
    idempotency_key: Option<&str>,
    request_hash: &str,
    f: F,
) -> Result<(T, IdempotencyStatus), ApiError>
where F: Future<Output = Result<T, ApiError>>,
      T: Serialize + DeserializeOwned,
{
    let Some(key) = idempotency_key else {
        return Ok((f.await?, IdempotencyStatus::None));
    };
    
    // Check existing
    let existing: Option<(String, String, Value)> = sqlx::query_as(
        "SELECT request_hash, response_body, response_code
         FROM platform.idempotency_keys WHERE key = $1"
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    
    if let Some((stored_hash, body, code)) = existing {
        if stored_hash != request_hash {
            return Err(ApiError::IdempotencyConflict);
        }
        let response: T = serde_json::from_value(body)?;
        return Ok((response, IdempotencyStatus::Replay));
    }
    
    let result = f.await?;
    
    sqlx::query(
        "INSERT INTO platform.idempotency_keys
         (key, request_hash, response_body, response_code)
         VALUES ($1, $2, $3, $4)"
    )
    .bind(key)
    .bind(request_hash)
    .bind(serde_json::to_value(&result)?)
    .bind(200)
    .execute(pool)
    .await?;
    
    Ok((result, IdempotencyStatus::Stored))
}
```

---

## Database Patterns

### Connection role (ADR-007)

```rust
// In configuration — NEVER use a superuser role
let pg_url = format!(
    "postgres://velocity_api:{}@{}/velocity",
    secret("velocity_api_password"),
    pg_host(),
);

// The role MUST be NOBYPASSRLS
// Operator verifies this at startup:
let bypass: bool = sqlx::query_scalar(
    "SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user"
).fetch_one(&pool).await?;

if bypass {
    panic!("velocity_api role has BYPASSRLS=true — RLS will not work. Fix the role.");
}
```

### Naming

```rust
fn pg_schema_name(org: &str, app: &str, domain: &str) -> String {
    format!("{}_{}_{}", sanitize(org), sanitize(app), sanitize(domain))
}

fn pg_table_name(object: &str, version: &str) -> String {
    format!("{}_{}", sanitize(object), sanitize(version))
}

fn sanitize(s: &str) -> String {
    s.to_lowercase().replace(['-', '.', ' '], "_")
}
```

### Partial unique constraints

Always partial — `WHERE deleted_at IS NULL`:

```sql
CREATE UNIQUE INDEX idx_{table}_{field}_active
ON {table} ({field})
WHERE deleted_at IS NULL;
```

### Optimistic locking

```sql
UPDATE {table}
SET ..., version = version + 1, updated_at = now(), updated_by = current_setting('app.current_user')
WHERE id = $1 AND version = $2;
```

If `rows_affected = 0`:
- Check if record exists → 409 Conflict (version mismatch)
- If not → 404 Not Found

---

## Outbox Pattern (ADR-002)

Outbox is part of the data-write transaction:

```rust
async fn write_with_outbox(
    tx: &mut PgConnection,
    schema: &ResolvedSchema,
    operation: Operation,
    record: &Value,
) -> Result<(), sqlx::Error> {
    // 1. Main table write
    let insert_sql = build_insert(schema, record);
    sqlx::query(&insert_sql).execute(&mut *tx).await?;
    
    // 2. Outbox write — same transaction
    if schema.search.tier == 3 {
        let outbox_sql = format!(
            "INSERT INTO {}.{}_outbox (op, entity_id, payload) VALUES ($1, $2, $3)",
            schema.pg_schema(), schema.pg_table()
        );
        sqlx::query(&outbox_sql)
            .bind(operation.to_string())
            .bind(record["id"].as_str().unwrap())
            .bind(record)
            .execute(&mut *tx).await?;
    }
    
    // Either both commit, or neither (atomicity)
    Ok(())
}
```

CDC worker reads outbox with `FOR UPDATE SKIP LOCKED` to allow concurrent workers without contention:

```rust
async fn cdc_loop(schema: ResolvedSchema, pool: PgPool, typesense: TypesenseClient) {
    loop {
        let mut tx = pool.begin().await.unwrap();
        
        let unpublished: Vec<OutboxRow> = sqlx::query_as(&format!(
            "SELECT id, op, entity_id, payload
             FROM {}.{}_outbox
             WHERE published_at IS NULL
             ORDER BY id
             LIMIT 100
             FOR UPDATE SKIP LOCKED",
            schema.pg_schema(), schema.pg_table()
        ))
        .fetch_all(&mut *tx).await.unwrap();
        
        if unpublished.is_empty() {
            tx.commit().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        
        // Send to Typesense
        for row in &unpublished {
            apply_to_typesense(&typesense, &schema, row).await.unwrap();
        }
        
        // Mark published
        let ids: Vec<i64> = unpublished.iter().map(|r| r.id).collect();
        sqlx::query(&format!(
            "UPDATE {}.{}_outbox SET published_at = now() WHERE id = ANY($1)",
            schema.pg_schema(), schema.pg_table()
        ))
        .bind(&ids)
        .execute(&mut *tx).await.unwrap();
        
        tx.commit().await.unwrap();
    }
}
```

---

## CEL Patterns

### Compile once, execute many

```rust
// At schema load time (informer event handler)
let compiled: Vec<CompiledRule> = schema.validations
    .iter()
    .filter_map(|v| match v {
        Validation::Cel { rule, message, max_execution_ms } => {
            Program::compile(rule).ok().map(|program| CompiledRule {
                program,
                message: message.clone(),
                max_ms: max_execution_ms.unwrap_or(10),
            })
        }
        _ => None,
    })
    .collect();
```

### Bounded execution (per ADR — review fix S2)

```rust
async fn evaluate(rule: &CompiledRule, input: &Value) -> Result<bool, ValidationError> {
    let mut ctx = Context::default();
    ctx.add_variable("self", input)?;
    
    let result = tokio::time::timeout(
        Duration::from_millis(rule.max_ms),
        async { rule.program.execute(&ctx) },
    ).await
    .map_err(|_| ValidationError::CelTimeout {
        max_ms: rule.max_ms,
        rule: rule.message.clone(),
    })?;
    
    Ok(result?.to_bool())
}
```

Never compile CEL on the hot path. Compile when schema is loaded; cache in `ResolvedSchema`.

---

## Observability Patterns

### Structured logging

```rust
tracing::info!(
    schema      = %schema.kind,
    operation   = %operation,
    entity_id   = %id,
    actor       = %identity.actor_id,
    duration_ms = duration.as_millis(),
    outcome     = "success",
    fail_mode   = ?audit_ctx.fail_mode,
    "operation completed"
);
```

Every log line must be valid JSON (`tracing-subscriber` with JSON format).

### Metric label cardinality

Approved label values only. **Never log entity IDs or user-supplied strings as labels.**

```
schema    bounded by SchemaDefinition.metadata.name (low cardinality)
operation create|read|update|delete|restore|export|query|search
outcome   success|error|denied|validation_error|not_found
actor_type human|service|operator|scheduler|anonymous
strategy  jwt|oidc|api_key|none|composite
```

`schema` label is included only on metrics where per-schema visibility is essential. Excluded from high-volume metrics.

### Trace propagation

```rust
let span = tracer.start_with_context(
    format!("{}.{}", schema.kind, operation),
    &parent_ctx,
);
span.set_attribute(KeyValue::new("schema.kind", schema.kind.clone()));
span.set_attribute(KeyValue::new("velocity.org", schema.org.clone()));

// Inject trace context into outgoing Kafka/HTTP for hooks
let traceparent = current_span_context().to_traceparent();
```

---

## Testing Strategy

### Unit tests

- `DdlBuilder`: for every field type + constraint type
- `QueryBuilder`: for every operator + invariant
- `ClaimResolver`: for every transform
- `PolicyTree`: merge logic, inheritance
- CEL: compile + evaluate per rule type
- Audit chain: hash computation
- Outbox: write atomicity (mock pool)

### Integration tests

Use `testcontainers` (Postgres, Redis, Kafka, Typesense).

```rust
#[tokio::test]
async fn test_create_writes_to_outbox() {
    let (pool, api) = setup_test_env().await;
    apply_schema(&api, PURCHASE_ORDER_SCHEMA).await;
    
    let response = api.post("/api/acme/.../v1")
        .header("Authorization", &test_token("ravi.kumar"))
        .json(&json!({ "po_number": "PO-00000001", "supplier_code": "TATA001" }))
        .send().await.unwrap();
    
    assert_eq!(response.status(), 201);
    
    // Verify outbox row exists in same transaction
    let outbox_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM acme_supply_chain_procurement.purchase_order_v1_outbox
         WHERE published_at IS NULL"
    ).fetch_one(&pool).await.unwrap();
    
    assert_eq!(outbox_count, 1);
}

#[tokio::test]
async fn test_redis_unavailable_denies_by_default() {
    let (_, api) = setup_test_env().await;
    apply_schema(&api, PURCHASE_ORDER_SCHEMA).await;
    
    // Kill Redis container
    stop_redis().await;
    
    let response = api.post("/api/acme/.../v1")
        .header("Authorization", &test_token("ravi.kumar"))
        .json(&json!({ "po_number": "PO-00000001" }))
        .send().await.unwrap();
    
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "REVOCATION_UNAVAILABLE");
}

#[tokio::test]
async fn test_cross_schema_join_requires_target_access() {
    let (_, api) = setup_test_env().await;
    apply_schema(&api, PURCHASE_ORDER_SCHEMA).await;
    apply_schema(&api, SUPPLIER_SCHEMA).await;
    
    // Actor has read on purchase-order but not on supplier
    let token = test_token_with_roles("anita", vec!["procurement-reader"]);
    
    let response = api.post("/api/acme/.../purchase-order/v1/query")
        .header("Authorization", &token)
        .json(&json!({
            "where": { "field": "status", "op": "eq", "value": "approved" },
            "include": ["supplier"]
        }))
        .send().await.unwrap();
    
    assert_eq!(response.status(), 403);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["code"], "CROSS_SCHEMA_ACCESS_DENIED");
}
```

### Webhook tests

Run against a real k3d cluster in CI:

```bash
k3d cluster create velocity-test
cargo test --test webhook_integration -- --nocapture
k3d cluster delete velocity-test
```

---

## Security Requirements (Non-Negotiable)

### Sensitive data never in logs

Fields with `sensitivity: financial | pii | confidential` are redacted before logging:

```rust
// CORRECT
tracing::info!(fields_changed = ?changed_field_names, "update completed");

// WRONG
tracing::info!(payload = ?full_payload, "update completed");
```

The `LogProcessor` handles outer redaction, but the API server must never log raw payloads.

### Audit chain integrity

Always use `platform.audit_insert()` stored procedure. Direct INSERT into `platform.audit_log` is blocked by GRANT.

### API key storage

Store only SHA256. Plaintext shown once at creation, never retrievable.

```rust
let plaintext = format!("vel_{}_{}", env, base64::encode(rand_bytes(32)));
let hash = sha256_hex(&plaintext);

// Store hash in DB
// Return plaintext to caller ONCE
```

### Input size limits

```rust
let app = Router::new()
    .layer(DefaultBodyLimit::max(10 * 1024 * 1024));  // 10 MB request body cap

// Per-field limit applied in validation:
if field.kind == FieldKind::String && payload[&field.name].as_str().unwrap().len() > 1_000_000 {
    return Err(ValidationError::FieldTooLarge);
}
```

---

## What NOT To Do

- Do not write per-schema route handlers — generic handlers only
- Do not write migration scripts by hand — `DdlBuilder` generates DDL
- Do not store plaintext secrets — SHA256 for API keys, argon2 for passwords
- Do not write raw SQL strings — use parameterized queries
- Do not interpolate user input into SQL — ever
- Do not compile CEL on the hot path — compile at schema load
- Do not use a superuser DB role — `velocity_api` is NOBYPASSRLS
- Do not store entity data in CRDs — CRDs are config only, data in Postgres
- Do not use HPA + VPA on the same Deployment — they conflict
- Do not use `unwrap()` in production code paths — propagate errors
- Do not log full request/response bodies — log metadata
- Do not skip the validating webhook — last line of defence
- Do not use `RwLock<RegistryInner>` — use `arc_swap::ArcSwap` (ADR-006)
- Do not skip outbox writes for Tier-3 schemas — silent index drift will happen
- Do not let any code path make local fail-mode decisions — use the matrix (ADR-003)
- Do not skip `SET LOCAL ROLE` before each transaction — RLS depends on it

---

## Implementation Sequence Per Phase

Within each phase (see `phases.md`):

1. Update type definitions in `velocity-types`
2. Write unit tests for the new types
3. Implement operator-side logic
4. Implement API-side logic
5. Write integration tests
6. Regenerate CRDs: `cargo run --bin generate-crds`
7. Update Helm chart if components added
8. Update CLAUDE.md if new conventions introduced

Every phase passes all tests before starting the next.

---

## Per-Phase Completion Checklist

- [ ] All unit tests pass (`cargo test`)
- [ ] All integration tests pass
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo audit` clean
- [ ] CRD manifests regenerated and committed
- [ ] Helm chart updated for new components
- [ ] `velocity apply` works end-to-end for new CRD types
- [ ] Structured logs validated (JSON parseable)
- [ ] Metrics present at `/metrics` for new operations
- [ ] Trace propagation verified for new code paths
- [ ] No new dependencies on banned patterns (RwLock, raw SQL, unwrap in handlers)
- [ ] CLAUDE.md updated for new conventions
- [ ] Runbook entry created for any new operational concerns
