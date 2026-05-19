---
title: Database schema
description: Every table in the platform schema, what it stores, and how it's indexed.
---

The `platform` schema owns every cross-tenant table Velocity needs: CRD mirrors, audit chain, event log, API keys, role bindings, sessions, idempotency cache, archive runs, purge requests, and the work queues that back background sweepers. **Per-tenant** record data lives in `{org}_{app}_{domain}.{object}_{version}` schemas provisioned by the hierarchy operator — those tables are not described here.

All `platform` tables are created by [migration 0001](/architecture/migrations/#0001_platform_schemasql). The DDL below is lifted verbatim — what ships in the repo is what runs in your cluster.

## Conventions

- **Idempotent DDL.** Every `CREATE TABLE` and `CREATE INDEX` uses `IF NOT EXISTS`; every `DO $$ ... $$` block guards inner work with `IF NOT EXISTS`. Re-applying the migrations is a no-op when state is current.
- **Partial unique indexes** over `WHERE deleted_at IS NULL` so soft-deleted rows can re-use the unique value. (Pattern is used in tenant tables; the `platform.api_keys` table applies the same idea over `WHERE revoked_at IS NULL`.)
- **UUIDv4 primary keys** for entity tables — `gen_random_uuid()` from `pgcrypto`, which the migration enables.
- **`TIMESTAMPTZ`** everywhere. No naked `TIMESTAMP` columns. Insert/update timestamps default to `now()`.
- **JSONB**, not JSON. We rely on binary representation for `@>` containment queries and key deduplication.

## CRD mirror tables

The kube informer is the source of truth for CRDs. These tables exist so operators can ad-hoc query "what schemas exist" without round-tripping to the apiserver.

### `platform.schema_definitions`

One row per `SchemaDefinition` CRD. Composite primary key is the natural path `(org, app, domain, object, version)`.

```sql
CREATE TABLE IF NOT EXISTS platform.schema_definitions (
    org              TEXT        NOT NULL,
    app              TEXT        NOT NULL,
    domain           TEXT        NOT NULL,
    object           TEXT        NOT NULL,
    version          TEXT        NOT NULL,
    namespace        TEXT        NOT NULL,
    name             TEXT        NOT NULL,
    pg_schema        TEXT        NOT NULL,
    pg_table         TEXT        NOT NULL,
    lifecycle        TEXT        NOT NULL DEFAULT 'stable',
    policy_hash      TEXT,
    spec             JSONB       NOT NULL,
    status           JSONB,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org, app, domain, object, version)
);

CREATE INDEX IF NOT EXISTS idx_schema_definitions_namespace
    ON platform.schema_definitions (namespace, name);
CREATE INDEX IF NOT EXISTS idx_schema_definitions_pg
    ON platform.schema_definitions (pg_schema, pg_table);
```

### `platform.field_definitions`

Flat field index, FK'd to the parent schema with `ON DELETE CASCADE` so a CRD removal cleans up its rows.

```sql
CREATE TABLE IF NOT EXISTS platform.field_definitions (
    org              TEXT NOT NULL,
    app              TEXT NOT NULL,
    domain           TEXT NOT NULL,
    object           TEXT NOT NULL,
    version          TEXT NOT NULL,
    name             TEXT NOT NULL,
    kind             TEXT NOT NULL,
    required         BOOLEAN NOT NULL DEFAULT false,
    unique_field     BOOLEAN NOT NULL DEFAULT false,
    indexed          BOOLEAN NOT NULL DEFAULT false,
    searchable       BOOLEAN NOT NULL DEFAULT false,
    sensitivity      TEXT,
    spec             JSONB NOT NULL,
    PRIMARY KEY (org, app, domain, object, version, name),
    FOREIGN KEY (org, app, domain, object, version)
        REFERENCES platform.schema_definitions(org, app, domain, object, version)
        ON DELETE CASCADE
);
```

## Event log (time machine)

`platform.event_log` is the [time-machine](/features/time-machine/) hot tier ([ADR-004](/adrs/#adr-004)). Every CREATE / UPDATE / DELETE / RESTORE goes here. UPDATEs carry a JSON Patch in `diff`; CREATEs carry the full record in `payload`; DELETEs carry `NULL`.

The table is **monthly range-partitioned by `occurred_at`**. The migration bootstraps the current month and the next month so writes never fail before the operator's partition manager runs.

```sql
CREATE TABLE IF NOT EXISTS platform.event_log (
    id            UUID        NOT NULL DEFAULT gen_random_uuid(),
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    schema_org    TEXT        NOT NULL,                 -- "{org}/{app}/{domain}/{object}/{version}"
    entity_id     UUID,
    operation     TEXT        NOT NULL,                 -- create|update|delete|restore|...
    actor         TEXT        NOT NULL,
    source        TEXT        NOT NULL DEFAULT 'api',   -- api|operator-sync|import|migration
    request_id    TEXT,
    diff          JSONB,                                -- JSON Patch for UPDATE
    payload       JSONB,                                -- full record for CREATE; NULL for DELETE
    reason        TEXT,                                 -- free-text from restore (added in 0004)
    PRIMARY KEY (id, occurred_at)
) PARTITION BY RANGE (occurred_at);
```

**Bootstrap partitions:**

```sql
DO $$
DECLARE
    cur_start DATE := date_trunc('month', now())::date;
    cur_end   DATE := (date_trunc('month', now()) + interval '1 month')::date;
    nxt_start DATE := cur_end;
    nxt_end   DATE := (date_trunc('month', now()) + interval '2 months')::date;
    cur_part  TEXT := format('event_log_%s', to_char(cur_start, 'YYYY_MM'));
    nxt_part  TEXT := format('event_log_%s', to_char(nxt_start, 'YYYY_MM'));
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = cur_part) THEN
        EXECUTE format(
            'CREATE TABLE platform.%I PARTITION OF platform.event_log FOR VALUES FROM (%L) TO (%L)',
            cur_part, cur_start, cur_end
        );
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = nxt_part) THEN
        EXECUTE format(
            'CREATE TABLE platform.%I PARTITION OF platform.event_log FOR VALUES FROM (%L) TO (%L)',
            nxt_part, nxt_start, nxt_end
        );
    END IF;
END
$$;
```

Indexes target the three query shapes the time-machine and audit replay actually issue:

```sql
CREATE INDEX IF NOT EXISTS idx_event_log_entity
    ON platform.event_log (entity_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_event_log_schema_time
    ON platform.event_log (schema_org, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_event_log_actor_time
    ON platform.event_log (actor, occurred_at DESC);
```

There is deliberately **no** index on `reason` — it's a read-back-only field; adding an index would just cost write bandwidth.

## Audit chain ([ADR-005](/adrs/#adr-005))

Two tables collaborate: `audit_log` is the append-only record, `audit_chain_state` is the singleton row whose UPDATE serialises chain construction so concurrent writers can't race.

### `platform.audit_log`

```sql
CREATE TABLE IF NOT EXISTS platform.audit_log (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor         TEXT        NOT NULL,
    action        TEXT        NOT NULL,                 -- create|read|update|delete|export|restore|...
    outcome       TEXT        NOT NULL,                 -- success|error|denied|...
    schema_org    TEXT,
    entity_id     UUID,
    payload       JSONB,
    prev_hash     TEXT,
    hash          TEXT        NOT NULL,
    fail_modes    JSONB,                                 -- ADR-003 fail-mode audit
    request_id    TEXT,
    reason        TEXT,
    ticket_ref    TEXT
);
```

Direct `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE` are **revoked from PUBLIC** in [migration 0003](/architecture/migrations/#0003_grantssql); the only way to write is `platform.audit_insert()` (see [stored procedures](/architecture/stored-procedures/)).

Indexes cover actor, schema, entity, and outcome timelines — the four facets of every audit lookup.

```sql
CREATE INDEX IF NOT EXISTS idx_audit_log_actor_time  ON platform.audit_log (actor, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_schema_time ON platform.audit_log (schema_org, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_entity_time ON platform.audit_log (entity_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_outcome     ON platform.audit_log (outcome, occurred_at DESC);
```

### `platform.audit_chain_state`

Singleton row keyed on `id = 1` with a CHECK constraint that forbids any other row. The `audit_insert()` proc updates this row to acquire the chain lock.

```sql
CREATE TABLE IF NOT EXISTS platform.audit_chain_state (
    id        INTEGER PRIMARY KEY DEFAULT 1,
    last_hash TEXT,
    CONSTRAINT audit_chain_state_singleton CHECK (id = 1)
);

INSERT INTO platform.audit_chain_state (id, last_hash)
    VALUES (1, NULL)
    ON CONFLICT (id) DO NOTHING;
```

## API keys

API key plaintext is **never stored** — only the SHA-256 hash. The `idx_api_keys_hash_active` partial unique index keeps lookups O(1) and makes "revoked + re-issued with same plaintext" naturally distinct (the unique only applies to active rows).

```sql
CREATE TABLE IF NOT EXISTS platform.api_keys (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT        NOT NULL,
    namespace       TEXT        NOT NULL,
    actor           TEXT        NOT NULL,
    actor_type      TEXT        NOT NULL,
    key_hash        TEXT        NOT NULL,           -- sha256 hex; plaintext never stored
    scopes          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    ip_allowlist    JSONB       NOT NULL DEFAULT '[]'::jsonb,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at      TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_hash_active
    ON platform.api_keys (key_hash)
    WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_api_keys_actor
    ON platform.api_keys (actor)
    WHERE revoked_at IS NULL;
```

## Role bindings

One row per `RoleBinding` CRD. The unique index on `(namespace, name)` matches the k8s identity; the partial index on `actor_id` keeps the per-actor lookup hot.

```sql
CREATE TABLE IF NOT EXISTS platform.role_bindings (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT        NOT NULL,
    namespace       TEXT        NOT NULL,
    actor_id        TEXT        NOT NULL,
    roles           JSONB       NOT NULL,
    scopes          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    granted_by      TEXT,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_role_bindings_actor
    ON platform.role_bindings (actor_id) WHERE revoked_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_role_bindings_name
    ON platform.role_bindings (namespace, name);
```

## Sessions

OIDC session state — the refresh token is **encrypted at-rest by the app** before insertion, not by Postgres.

```sql
CREATE TABLE IF NOT EXISTS platform.sessions (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id        TEXT        NOT NULL,
    issuer          TEXT        NOT NULL,
    refresh_token   TEXT,
    id_token_claims JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_sessions_actor
    ON platform.sessions (actor_id) WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_expiry
    ON platform.sessions (expires_at) WHERE revoked_at IS NULL;
```

## Idempotency keys

24-hour TTL. The `(key, request_hash)` pair detects "same key, different body" and returns 409. The `created_at` index supports the GC sweeper.

```sql
CREATE TABLE IF NOT EXISTS platform.idempotency_keys (
    key            TEXT        PRIMARY KEY,
    request_hash   TEXT        NOT NULL,
    response_body  JSONB,
    response_code  INTEGER     NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_idempotency_keys_created
    ON platform.idempotency_keys (created_at);
```

## Archive runs & purge requests

Operational state for the [archive worker](/features/archive/). `archive_runs` is the historical log; `purge_requests` is the queue the operator walks to perform two-person-rule deletes.

```sql
CREATE TABLE IF NOT EXISTS platform.archive_runs (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    policy_name     TEXT        NOT NULL,
    policy_namespace TEXT       NOT NULL,
    schema_org      TEXT        NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ,
    records         BIGINT,
    bytes           BIGINT,
    destination     TEXT,
    outcome         TEXT        NOT NULL DEFAULT 'running',
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_archive_runs_schema
    ON platform.archive_runs (schema_org, started_at DESC);

CREATE TABLE IF NOT EXISTS platform.purge_requests (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    schema_org        TEXT        NOT NULL,
    older_than        TIMESTAMPTZ NOT NULL,
    estimated_records BIGINT,
    requested_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    approved_at       TIMESTAMPTZ,
    approved_by       TEXT,
    purged_at         TIMESTAMPTZ,
    purged_records    BIGINT,
    reason            TEXT
);

CREATE INDEX IF NOT EXISTS idx_purge_requests_pending
    ON platform.purge_requests (requested_at DESC)
    WHERE purged_at IS NULL;
```

## Typesense reap queue

A durable work queue for the blue-green search collection reap. After an alias flip the operator must drop the old concrete collection — but only after a grace period that lets in-flight queries finish. The original implementation used `tokio::time::sleep` on a detached task; an operator restart during the grace window leaked the old concrete forever. This table makes the work crash-safe.

```sql
CREATE TABLE IF NOT EXISTS platform.pending_typesense_reaps (
    id            BIGSERIAL PRIMARY KEY,
    concrete_name TEXT NOT NULL UNIQUE,
    alias_name    TEXT NOT NULL,
    schema_uid    TEXT NOT NULL,
    enqueued_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    reap_after    TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_typesense_reaps_due
    ON platform.pending_typesense_reaps (reap_after);
```

The sweeper claims due rows via `SELECT ... WHERE reap_after <= now() FOR UPDATE SKIP LOCKED`, so multiple replicas don't double-claim.

## Anomaly detection

Two tables collaborate. `anomaly_scan_state` is a high-watermark singleton; the scanner only inspects rows with `(occurred_at, id) > (cursor_ts, cursor_id)`, which gives strict total order even when two audit rows land in the same microsecond. `anomaly_alerts` is the detection store, with hourly dedupe via a partial unique index.

```sql
CREATE TABLE IF NOT EXISTS platform.anomaly_scan_state (
    id                          INTEGER     PRIMARY KEY DEFAULT 1,
    last_scanned_occurred_at    TIMESTAMPTZ,
    last_scanned_id             UUID,
    last_scanned_at             TIMESTAMPTZ,
    CONSTRAINT anomaly_scan_state_singleton CHECK (id = 1)
);

CREATE TABLE IF NOT EXISTS platform.anomaly_alerts (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    detected_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    rule            TEXT        NOT NULL,   -- bulk_reader|after_hours|repeated_denials
    actor           TEXT,
    schema_org      TEXT,
    severity        TEXT        NOT NULL DEFAULT 'warning',
    detail          JSONB       NOT NULL,
    window_start    TIMESTAMPTZ NOT NULL,
    window_end      TIMESTAMPTZ NOT NULL,
    delivered       BOOLEAN     NOT NULL DEFAULT false,
    delivered_at    TIMESTAMPTZ
);

-- Hourly dedupe: same (rule, actor, schema) within the same UTC hour collapses.
CREATE UNIQUE INDEX IF NOT EXISTS uniq_anomaly_alerts_dedupe
    ON platform.anomaly_alerts (
        rule,
        COALESCE(actor, ''),
        COALESCE(schema_org, ''),
        date_trunc('hour', (detected_at AT TIME ZONE 'UTC'))
    );
```

**Why the `AT TIME ZONE 'UTC'` cast in the unique index?** `date_trunc('hour', TIMESTAMPTZ)` is only `STABLE` — the result depends on the session's `TimeZone` GUC, which means Postgres won't allow it in a unique index. The cast to a plain `TIMESTAMP` makes the expression `IMMUTABLE`, which is the index requirement.
