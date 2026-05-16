-- 0001_platform_schema.sql
--
-- The `platform` schema owns every cross-tenant table Velocity needs:
-- the CRD mirror, the audit chain, idempotency keys, sessions, etc.
-- Per-org data lives in {org}_{app}_{domain} schemas provisioned by the
-- HierarchyOperator (Phase 0 Step 4); this file does not touch those.
--
-- Idempotent: safe to re-apply (CREATE IF NOT EXISTS / DO blocks for indexes).

BEGIN;

CREATE EXTENSION IF NOT EXISTS pgcrypto;   -- digest() for audit_insert (ADR-005)

CREATE SCHEMA IF NOT EXISTS platform;

-- ─── CRD mirrors (informational) ────────────────────────────────────────────
-- The kube informer is the source of truth; these tables exist so operators
-- can ad-hoc query "what schemas exist" without round-tripping to k8s API.

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

CREATE TABLE IF NOT EXISTS platform.field_definitions (
    org              TEXT NOT NULL,
    app              TEXT NOT NULL,
    domain           TEXT NOT NULL,
    object           TEXT NOT NULL,
    version          TEXT NOT NULL,
    name             TEXT NOT NULL,
    kind             TEXT NOT NULL,        -- string|integer|number|...
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

-- ─── Event log — partitioned monthly by occurred_at ─────────────────────────
-- ADR-004 hot tier. The operator manages monthly partition creation/drop.
-- We bootstrap the parent + this month + next month so writes never fail
-- before the partition manager runs.

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
    PRIMARY KEY (id, occurred_at)
) PARTITION BY RANGE (occurred_at);

-- Bootstrap partitions: current month + next month
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

CREATE INDEX IF NOT EXISTS idx_event_log_entity
    ON platform.event_log (entity_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_event_log_schema_time
    ON platform.event_log (schema_org, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_event_log_actor_time
    ON platform.event_log (actor, occurred_at DESC);

-- ─── Audit log + chain state (ADR-005) ──────────────────────────────────────

CREATE TABLE IF NOT EXISTS platform.audit_log (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor         TEXT        NOT NULL,
    action        TEXT        NOT NULL,                 -- create|read|update|delete|export|restore|...
    outcome       TEXT        NOT NULL,                 -- success|error|denied|...
    schema_org    TEXT,                                  -- "{org}/{app}/{domain}/{object}/{version}"
    entity_id     UUID,
    payload       JSONB,
    prev_hash     TEXT,
    hash          TEXT        NOT NULL,
    fail_modes    JSONB,                                 -- audit of FailMode::resolve outcomes (ADR-003)
    request_id    TEXT,
    reason        TEXT,
    ticket_ref    TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_log_actor_time
    ON platform.audit_log (actor, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_schema_time
    ON platform.audit_log (schema_org, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_entity_time
    ON platform.audit_log (entity_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_audit_log_outcome
    ON platform.audit_log (outcome, occurred_at DESC);

-- Singleton row that serializes chain construction (see ADR-005).
CREATE TABLE IF NOT EXISTS platform.audit_chain_state (
    id        INTEGER PRIMARY KEY DEFAULT 1,
    last_hash TEXT,
    CONSTRAINT audit_chain_state_singleton CHECK (id = 1)
);

INSERT INTO platform.audit_chain_state (id, last_hash)
    VALUES (1, NULL)
    ON CONFLICT (id) DO NOTHING;

-- ─── API keys (Phase 2c, but the table lands now) ───────────────────────────

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

-- ─── Role bindings ──────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS platform.role_bindings (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT        NOT NULL,
    namespace       TEXT        NOT NULL,
    actor_id        TEXT        NOT NULL,
    roles           JSONB       NOT NULL,            -- ["procurement-reader", ...]
    scopes          JSONB       NOT NULL DEFAULT '[]'::jsonb,
    granted_by      TEXT,
    expires_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_role_bindings_actor
    ON platform.role_bindings (actor_id)
    WHERE revoked_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_role_bindings_name
    ON platform.role_bindings (namespace, name);

-- ─── Sessions (OIDC) ────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS platform.sessions (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    actor_id        TEXT        NOT NULL,
    issuer          TEXT        NOT NULL,
    refresh_token   TEXT,                             -- encrypted at-rest by app
    id_token_claims JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,
    revoked_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_sessions_actor
    ON platform.sessions (actor_id)
    WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_expiry
    ON platform.sessions (expires_at)
    WHERE revoked_at IS NULL;

-- ─── Idempotency keys ───────────────────────────────────────────────────────
-- Stored for 24h (purged by a cron job).

CREATE TABLE IF NOT EXISTS platform.idempotency_keys (
    key            TEXT        PRIMARY KEY,
    request_hash   TEXT        NOT NULL,
    response_body  JSONB,
    response_code  INTEGER     NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_idempotency_keys_created
    ON platform.idempotency_keys (created_at);

-- ─── Archive runs ───────────────────────────────────────────────────────────

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

-- ─── Purge requests ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS platform.purge_requests (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    schema_org      TEXT        NOT NULL,
    older_than      TIMESTAMPTZ NOT NULL,
    estimated_records BIGINT,
    requested_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    approved_at     TIMESTAMPTZ,
    approved_by     TEXT,
    purged_at       TIMESTAMPTZ,
    purged_records  BIGINT,
    reason          TEXT
);

CREATE INDEX IF NOT EXISTS idx_purge_requests_pending
    ON platform.purge_requests (requested_at DESC)
    WHERE purged_at IS NULL;

COMMIT;
