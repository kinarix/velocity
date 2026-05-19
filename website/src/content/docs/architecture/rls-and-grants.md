---
title: RLS and grants
description: The two-role model, NOBYPASSRLS enforcement, and per-tenant grants.
---

Velocity runs Postgres with two roles, both `NOSUPERUSER` and `NOBYPASSRLS`. RLS is therefore a hard backstop, not a paper one — the database itself enforces it, and the API server's startup health check refuses to run if its role somehow drifts back to `BYPASSRLS=true`.

This page covers what each role can do, why we run it that way, and the per-tenant `SET LOCAL ROLE` pattern that scopes each transaction.

## The two roles

Defined in [`db/init/01-roles.sql`](https://github.com/kinarix/velocity/blob/main/db/init/01-roles.sql).

```sql
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'velocity_api') THEN
        CREATE ROLE velocity_api LOGIN PASSWORD 'velocity_api_dev'
            NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOINHERIT;
    END IF;
END
$$;

ALTER ROLE velocity_api NOSUPERUSER NOBYPASSRLS;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'velocity_operator') THEN
        CREATE ROLE velocity_operator LOGIN PASSWORD 'velocity_operator_dev'
            NOSUPERUSER NOBYPASSRLS CREATEROLE;
    END IF;
END
$$;

GRANT CONNECT ON DATABASE velocity TO velocity_api, velocity_operator;
GRANT CREATE  ON DATABASE velocity TO velocity_operator;
```

| Role | Used by | Privileges | Notes |
|------|---------|------------|-------|
| `velocity_api` | API server runtime | `SELECT`/`INSERT`/`UPDATE` on app tables; **no** `INSERT` on `audit_log` | `NOINHERIT` so domain role memberships only count when explicitly `SET ROLE`'d. |
| `velocity_operator` | Operator reconcilers, migrations | DDL on `platform.*`, `CREATE SCHEMA` per tenant, `CREATE ROLE` for tenant readers/writers/admins | `CREATEROLE` is the bounded "platform admin" surface; still `NOSUPERUSER`/`NOBYPASSRLS`. |

Passwords above are dev defaults — production deployments inject secrets into a Kubernetes `Secret` and mount them as env vars consumed by the api/operator Pods.

## `NOBYPASSRLS` is verified at startup ([ADR-007](/adrs/#adr-007))

```rust
let bypass: bool = sqlx::query_scalar(
    "SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user"
).fetch_one(&pool).await?;

if bypass {
    panic!("velocity_api role has BYPASSRLS=true — RLS will not work. Fix the role.");
}
```

If a DBA accidentally toggles `BYPASSRLS` on the role, the API server crashes on next start rather than silently leaking data across tenants. This is why we can rely on RLS for tenant isolation — not because we hope the policies are right, but because the platform won't run if its database identity could bypass them.

## Per-transaction `SET LOCAL ROLE`

The `velocity_api` login role has no direct access to tenant schemas (`acme_supply_chain_procurement`, etc.). Tenant access is granted to **domain roles** — `acme_supply_chain_procurement_reader`, `..._writer`, `..._admin` — which the operator creates when a `Domain` CRD is provisioned. The API server selects the appropriate domain role per request and switches into it with `SET LOCAL ROLE`:

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

    // Per-domain role — RLS policies key off this.
    sqlx::query(&format!("SET LOCAL ROLE {domain_role}"))
        .execute(&mut *tx).await?;

    // Actor context for RLS and audit.
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

Three properties matter:

1. **`SET LOCAL`** — scoped to the transaction. When the connection returns to the pool, the role and GUCs reset. A request that touches domain A cannot leak into a request that touches domain B even if they share a pooled connection.
2. **`app.current_user`** is a session GUC, not a column on the tenant table. RLS policies read it via `current_setting('app.current_user')` and use it for predicates like `created_by = current_setting(...)` or "this user owns this row."
3. **`app.current_store_id`** (and any other attribute GUC) is set from the resolved identity's claim map, not from request headers. The mapping from JWT claim → GUC is declared on the `AuthStrategy` CRD; the API server never trusts an unvalidated header.

The full pattern, including which RLS policies are generated for a typical tenant table, lives in the [security](/security/) page.

## Grants on the `platform` schema

[Migration 0003](/architecture/migrations/#0003_grantssql) is the source of truth for least-privilege grants on `platform.*`. The headline points:

### Block direct audit writes

```sql
REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON platform.audit_log FROM PUBLIC;
```

`velocity_api` is never granted these. The only path to `platform.audit_log` is `platform.audit_insert()`, which runs `SECURITY DEFINER` as the migration owner.

### `velocity_api` — read most, write little {#audit-write-grant}

```sql
GRANT USAGE ON SCHEMA platform TO velocity_api;

-- Read-only tables
GRANT SELECT ON
    platform.schema_definitions,
    platform.field_definitions,
    platform.role_bindings,
    platform.api_keys,
    platform.archive_runs,
    platform.purge_requests
TO velocity_api;

-- Tables the API writes during normal request processing
GRANT SELECT, INSERT, UPDATE ON
    platform.event_log,
    platform.idempotency_keys,
    platform.sessions
TO velocity_api;

-- Audit log: read OK, write ONLY through the stored proc
GRANT SELECT ON platform.audit_log TO velocity_api;
GRANT EXECUTE ON FUNCTION platform.audit_insert(
    TEXT, TEXT, TEXT, TEXT, UUID, JSONB, JSONB, TEXT, TEXT, TEXT
) TO velocity_api;
GRANT EXECUTE ON FUNCTION platform.audit_verify_window(TIMESTAMPTZ, TIMESTAMPTZ) TO velocity_api;

-- Future event_log partitions inherit perms via DEFAULT PRIVILEGES.
ALTER DEFAULT PRIVILEGES IN SCHEMA platform
    GRANT SELECT, INSERT ON TABLES TO velocity_api;
```

The `ALTER DEFAULT PRIVILEGES` line covers the monthly `event_log_YYYY_MM` partitions that the partition manager creates over time — they pick up the grant automatically.

### `velocity_operator` — DDL + bookkeeping

```sql
GRANT USAGE, CREATE ON SCHEMA platform TO velocity_operator;

GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE ON
    platform.schema_definitions,
    platform.field_definitions,
    platform.api_keys,
    platform.role_bindings,
    platform.archive_runs,
    platform.purge_requests
TO velocity_operator;

GRANT SELECT, INSERT, UPDATE ON
    platform.audit_chain_state
TO velocity_operator;

GRANT SELECT ON platform.audit_log, platform.event_log TO velocity_operator;

ALTER DEFAULT PRIVILEGES IN SCHEMA platform
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO velocity_operator;
```

The operator can read but **cannot write** `audit_log` directly either — it can only update `audit_chain_state` (which the proc needs in order to compute the chain). That keeps the "only way to append to audit" property even from the platform's own admin role.

### Sequence grants

```sql
ALTER DEFAULT PRIVILEGES IN SCHEMA platform
    GRANT USAGE, SELECT ON SEQUENCES TO velocity_api, velocity_operator;
```

`BIGSERIAL` on tenant outbox tables and on `platform.pending_typesense_reaps` needs sequence usage — this default grant covers any future sequence added to `platform.*` without a follow-up migration.

## What this protects against

| Threat | Mitigation |
|--------|-----------|
| Compromised API server replica | RLS scoped to `SET LOCAL ROLE`; cannot see other tenants' rows; cannot tamper with audit chain. |
| SQL injection in a handler | All queries are parameterised; even a successful injection runs under the domain role's RLS. |
| Insider modifying audit chain | `INSERT/UPDATE/DELETE` revoked on `audit_log`; tampering only possible via direct connection as superuser, which the cluster's pg_hba should not permit from app subnets. |
| BYPASSRLS regression | API server health check refuses to start; deployment rollouts fail loud. |

What this **does not** protect against: a Postgres superuser (DBA) acting in bad faith. They can drop the policies, mint new audit rows with valid-looking hashes, etc. Pair the database controls with separation of duties on the human side — production access logged, ticketed, and reviewed.
