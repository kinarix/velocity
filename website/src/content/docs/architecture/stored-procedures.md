---
title: Stored procedures
description: The audit-chain writer and verifier — the only sanctioned access to platform.audit_log.
---

Velocity ships exactly two stored procedures. Both live in the `platform` schema, both are `SECURITY DEFINER`, both pin `search_path` so an attacker who fakes a `platform`-shadowing object in a session-mutable schema can't get one of these procs to call into their code.

There is no DSL-style "do everything in PL/pgSQL" surface. These two functions are the audit boundary — every other write the API server makes is plain parameterised SQL.

## `audit_insert()`

Defined in [migration 0002](/architecture/migrations/#0002_audit_insertsql). This is the **only** entry point for writing to `platform.audit_log` — direct `INSERT` is revoked from `velocity_api` in [migration 0003](/architecture/migrations/#0003_grantssql).

```sql
CREATE OR REPLACE FUNCTION platform.audit_insert(
    p_actor       TEXT,
    p_action      TEXT,
    p_outcome     TEXT,
    p_schema_org  TEXT,
    p_entity_id   UUID,
    p_payload     JSONB,
    p_fail_modes  JSONB DEFAULT NULL,
    p_request_id  TEXT  DEFAULT NULL,
    p_reason      TEXT  DEFAULT NULL,
    p_ticket_ref  TEXT  DEFAULT NULL
) RETURNS UUID
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = platform, pg_catalog
AS $$
DECLARE
    v_id        UUID := gen_random_uuid();
    v_prev_hash TEXT;
    v_new_hash  TEXT;
    v_now       TIMESTAMPTZ := now();
BEGIN
    -- Serialize on the singleton row. The UPDATE acquires a row-exclusive lock
    -- so concurrent callers are forced into a single chain order.
    UPDATE platform.audit_chain_state
       SET last_hash = last_hash    -- intentional no-op; we need the lock
     WHERE id = 1
    RETURNING last_hash INTO v_prev_hash;

    v_new_hash := encode(
        public.digest(
            v_id::text || v_now::text || p_actor || p_action || p_outcome ||
            coalesce(p_schema_org, '') || coalesce(p_entity_id::text, '') ||
            coalesce(p_payload::text, '') || coalesce(v_prev_hash, ''),
            'sha256'
        ),
        'hex'
    );

    INSERT INTO platform.audit_log (
        id, occurred_at, actor, action, outcome, schema_org,
        entity_id, payload, prev_hash, hash,
        fail_modes, request_id, reason, ticket_ref
    ) VALUES (
        v_id, v_now, p_actor, p_action, p_outcome, p_schema_org,
        p_entity_id, p_payload, v_prev_hash, v_new_hash,
        p_fail_modes, p_request_id, p_reason, p_ticket_ref
    );

    UPDATE platform.audit_chain_state
       SET last_hash = v_new_hash
     WHERE id = 1;

    RETURN v_id;
END;
$$;
```

### What the body actually does

1. **Lock the chain.** `UPDATE platform.audit_chain_state SET last_hash = last_hash WHERE id = 1` is an intentional no-op write. The point is the row-exclusive lock it acquires — every concurrent `audit_insert` queues on this row, which forces a strict global order on chain links.
2. **Read the previous hash** via the `RETURNING last_hash` clause on that same UPDATE.
3. **Compute `hash = sha256(id || occurred_at || actor || action || outcome || schema_org || entity_id || payload || prev_hash)`** using `pgcrypto`'s `public.digest()`. NULLs in any of the optional fields are coerced to empty strings via `coalesce` so the hash is deterministic.
4. **Insert the row** with both `prev_hash` and `hash` populated.
5. **Update the singleton** so the next caller sees the new tail.

### Why `SECURITY DEFINER`?

`velocity_api` does not have direct write privilege on `audit_log` ([grants](/architecture/rls-and-grants/#audit-write-grant)). The function runs as the migration owner (typically `velocity_operator`), which does have INSERT. `SECURITY DEFINER` + the `search_path` pin is how we let an unprivileged caller perform a privileged write through a narrow contract — the function's parameter shape is the only thing they can manipulate.

### Why pin `search_path`?

```sql
SET search_path = platform, pg_catalog
```

A user with `CREATE` on a session-mutable schema could otherwise create a fake `digest()` function that shadows `public.digest`, then trick the `SECURITY DEFINER` function into running attacker code with elevated privilege. Pinning `search_path` removes the lookup ambiguity. The call site is also fully qualified — `public.digest(...)` — as a defence-in-depth.

### Why not just use a trigger?

A `BEFORE INSERT` trigger that computed the hash and serialised on `audit_chain_state` would also work. The proc form is preferred because:

- The application has to call the proc explicitly — there's no path that "accidentally writes to `audit_log` and gets hashed automatically." If you see an `INSERT INTO platform.audit_log` in code review, that's a bug.
- The `REVOKE INSERT` on the table makes "what's writable" obvious to anyone reading grants.

## `audit_verify_window()`

Defined alongside `audit_insert()` in [migration 0002](/architecture/migrations/#0002_audit_insertsql). Recomputes the hash for every row in a time window so a CLI or auditor can detect tampering.

```sql
CREATE OR REPLACE FUNCTION platform.audit_verify_window(
    p_from TIMESTAMPTZ,
    p_to   TIMESTAMPTZ
) RETURNS TABLE (
    id             UUID,
    occurred_at    TIMESTAMPTZ,
    stored_hash    TEXT,
    computed_hash  TEXT
)
    LANGUAGE sql
    STABLE
    SECURITY DEFINER
    SET search_path = platform, pg_catalog
AS $$
    SELECT
        a.id,
        a.occurred_at,
        a.hash AS stored_hash,
        encode(
            public.digest(
                a.id::text || a.occurred_at::text || a.actor || a.action || a.outcome ||
                coalesce(a.schema_org, '') || coalesce(a.entity_id::text, '') ||
                coalesce(a.payload::text, '') || coalesce(a.prev_hash, ''),
                'sha256'
            ),
            'hex'
        ) AS computed_hash
    FROM platform.audit_log a
    WHERE a.occurred_at >= p_from AND a.occurred_at < p_to;
$$;
```

The hash formula is identical to `audit_insert()` — that's the point: rows where `stored_hash != computed_hash` were tampered with after insertion.

### How `velocity audit verify` uses it

```bash
$ velocity audit verify --from 2026-05-01 --to 2026-05-19
checking 12,847 rows...
all rows verified ✓

$ velocity audit verify --from 2026-04-01 --to 2026-05-01
checking 38,201 rows...
TAMPER: row 5f2c... at 2026-04-12 14:33:09 — stored != computed
1 row tampered
```

The CLI shells out to a query like:

```sql
SELECT id, occurred_at FROM platform.audit_verify_window($1, $2)
WHERE stored_hash IS DISTINCT FROM computed_hash;
```

A non-empty result is an integrity failure. The function is `STABLE`, not `IMMUTABLE`, because it reads the table — but each row computation is deterministic.

### What this catches, what it doesn't

Detects: any `UPDATE` or `DELETE` against `platform.audit_log` that bypasses `audit_insert()`. Both are revoked from `velocity_api`, so the only way they happen is a DBA or a compromised `velocity_operator` role.

Does **not** detect: an attacker with `INSERT` on the table who appends fake rows with valid hashes (they can mint a new chain segment). The chain only proves "this segment is internally consistent," not "this is the only segment." Pair audit verification with monitoring on the `audit_chain_state.last_hash` value — if it jumps non-monotonically, the chain was forked.

## What's *not* a stored procedure

Things that look like they might be procs but aren't:

- **DDL provisioning** (per-tenant `CREATE SCHEMA`, `CREATE TABLE`, `CREATE INDEX`) — emitted from the operator in Rust, via `velocity-operator/src/ddl_builder.rs`. The DDL is parameterised at the application layer; Postgres sees fully-formed statements.
- **The outbox publisher** ([ADR-002](/adrs/#adr-002)) — Rust worker reading `{schema}.{table}_outbox` with `FOR UPDATE SKIP LOCKED`.
- **The anomaly scanner** — Rust task in the operator that reads `platform.audit_log` past the high-watermark cursor.
- **The Typesense reap sweeper** — Rust task that walks `platform.pending_typesense_reaps WHERE reap_after <= now()`.

We deliberately keep PL/pgSQL surface minimal: harder to test, harder to deploy, harder to upgrade. The audit chain is the single exception because its **integrity** depends on atomic "lock + read prev_hash + compute + insert + update tail" — pulling that out of one transaction is asking for a forked chain on a network hiccup.
