---
title: Migrations
description: Index of every migration that brings a Velocity database from empty to current.
---

Migrations live in [`migrations/`](https://github.com/kinarix/velocity/tree/main/migrations) at the repo root, numbered sequentially. Each file is **idempotent** — `CREATE ... IF NOT EXISTS`, `ON CONFLICT DO NOTHING`, guarded `DO $$` blocks. Re-applying any of them against an already-current cluster is a no-op.

There is **no separate migration runner binary**. The operator applies them in order at startup, recording high-water-mark in `platform.schema_migrations` (created lazily). For local dev, `make db-bootstrap` runs the same path against a containerised Postgres.

Conventions:

- **Numbered sequentially**, never renumbered or reordered. Once a migration is merged, the only way to change behaviour is a new migration.
- **`BEGIN; ... COMMIT;`** wraps the entire file so a failure rolls back partial state.
- **No `DROP` of in-use objects** without a paired column-rename + backfill + cutover migration. Breaking changes are managed via the `velocity.sh/breaking-change: approved` annotation gate, not by sneaking destructive DDL into a migration.

## `0001_platform_schema.sql`

Creates the `platform` schema and every cross-tenant table:

- `platform.schema_definitions`, `platform.field_definitions` — CRD mirrors.
- `platform.event_log` — time-machine hot tier (monthly range partitions, bootstrapped for current + next month).
- `platform.audit_log`, `platform.audit_chain_state` — append-only audit chain + singleton lock row.
- `platform.api_keys`, `platform.role_bindings`, `platform.sessions` — auth state.
- `platform.idempotency_keys` — 24-hour cache.
- `platform.archive_runs`, `platform.purge_requests` — archive bookkeeping.

Also enables `pgcrypto` for `digest()` and `gen_random_uuid()`.

See [database schema](/architecture/database/) for full DDL.

## `0002_audit_insert.sql`

Defines the two audit-chain stored procedures:

- `platform.audit_insert(...)` — `SECURITY DEFINER`; the only sanctioned writer for `platform.audit_log`. Serialises on `audit_chain_state`, hashes `(id, occurred_at, actor, action, outcome, schema_org, entity_id, payload, prev_hash)` with SHA-256, links via `prev_hash`/`hash`.
- `platform.audit_verify_window(p_from, p_to)` — recomputes each row hash in a window so tampered rows surface as `stored != computed`.

Both pin `search_path = platform, pg_catalog` to defeat shadowing attacks on `digest()`.

See [stored procedures](/architecture/stored-procedures/) for the full bodies and rationale.

## `0003_grants.sql`

Least-privilege grants on `platform.*`:

- `REVOKE INSERT/UPDATE/DELETE/TRUNCATE ON platform.audit_log FROM PUBLIC` — direct writes blocked for everyone, including `velocity_api`.
- `velocity_api` — `SELECT` on most tables; `SELECT/INSERT/UPDATE` on `event_log`, `idempotency_keys`, `sessions`; `EXECUTE` on `audit_insert` + `audit_verify_window`.
- `velocity_operator` — DDL on `platform.*`; reads `audit_log` but cannot write it directly (only `audit_chain_state`).
- `ALTER DEFAULT PRIVILEGES` so future tables and sequences inherit the right grants without follow-up migrations.

See [RLS and grants](/architecture/rls-and-grants/) for the full grant matrix.

## `0004_event_log_reason.sql`

Adds `reason TEXT` to `platform.event_log` so the restore endpoint can stamp the operator's free-text rationale alongside the event. Nullable (most events have no reason); not indexed (read-back only).

```sql
ALTER TABLE platform.event_log
    ADD COLUMN IF NOT EXISTS reason TEXT;
```

This is the smallest migration in the repo and an example of how additive column changes ship — no rebuild, no rewrite, just an `ADD COLUMN IF NOT EXISTS`.

## `0005_pending_typesense_reaps.sql`

Adds a durable work queue for the Phase 5d blue-green Typesense alias-flip + concrete-collection reap.

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

The original implementation used `tokio::time::sleep` on a detached task; an operator restart during the grace window leaked the old concrete forever. Persisting the work makes the sweeper crash-safe — on restart it rediscovers everything not yet reaped, plus anything past its `reap_after`. Sweeper claims work via `FOR UPDATE SKIP LOCKED`.

## `0006_anomaly_alerts.sql`

Phase 6c — audit-driven anomaly detection. Adds two tables:

- `platform.anomaly_scan_state` — high-watermark singleton. The scanner walks `audit_log` rows strictly greater than `(last_scanned_occurred_at, last_scanned_id)`. The composite cursor is necessary because `audit_log.id` is a v4 random UUID, so it cannot be compared on its own for arrival order.
- `platform.anomaly_alerts` — detections, with hourly dedupe via a partial unique index over `(rule, COALESCE(actor,''), COALESCE(schema_org,''), date_trunc('hour', detected_at AT TIME ZONE 'UTC'))`.

The `AT TIME ZONE 'UTC'` cast is load-bearing — `date_trunc('hour', TIMESTAMPTZ)` is only `STABLE` (it depends on the session's `TimeZone` GUC) and therefore cannot be indexed; casting to plain `TIMESTAMP` makes the expression `IMMUTABLE`.

Grants here are tenant-shaped, not platform-wide:

```sql
GRANT SELECT, INSERT, UPDATE ON platform.anomaly_alerts     TO velocity_operator;
GRANT SELECT, UPDATE         ON platform.anomaly_scan_state TO velocity_operator;
GRANT SELECT                 ON platform.anomaly_alerts,
                                platform.anomaly_scan_state TO velocity_api;
```

## How a new migration ships

1. **Number it** `NNNN_short_slug.sql` — next sequential, no gaps.
2. **Wrap in `BEGIN; ... COMMIT;`**. If you can't make the whole file atomic (e.g., `CREATE INDEX CONCURRENTLY` cannot run in a transaction), split it into two files: the transactional setup, then the concurrent step.
3. **Guard everything** with `IF NOT EXISTS` / `IF EXISTS` / `DO $$` checks. Re-applying must be a no-op.
4. **Never edit a merged migration.** Once it's in `main`, the only way to change its effect is a new migration that ALTERs or DROPs the prior object.
5. **No data migration in DDL files.** If you need to backfill a column, write a short Rust job that batches the work; the DDL file just adds the (nullable) column.

The operator's startup path applies pending migrations in order. A failure rolls back the partial file and the operator exits non-zero — Kubernetes will restart it, and the next attempt has the same broken file to apply, so you'll see it in `kubectl logs` rather than in silent half-applied state.
