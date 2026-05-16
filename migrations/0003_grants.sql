-- 0003_grants.sql
--
-- Least-privilege grants on the `platform` schema.
--
-- velocity_api  — the runtime role used by the API server. NOBYPASSRLS
--                 (verified at startup; see ADR-007). Cannot INSERT into
--                 platform.audit_log directly — must call audit_insert().
-- velocity_operator — used by reconcilers. Owns DDL for platform.* mirrors
--                     and creates per-domain schemas/roles in the
--                     hierarchy operator.
--
-- Idempotent: GRANT/REVOKE are no-ops if already in the requested state.

BEGIN;

-- ─── Block direct writes to audit_log from everyone ─────────────────────────
REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON platform.audit_log FROM PUBLIC;

-- ─── velocity_api: read most things, write only via audit_insert ────────────
GRANT USAGE ON SCHEMA platform TO velocity_api;

-- Read-mostly tables
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

-- Audit log: read OK, write ONLY through the stored proc (REVOKE above blocks direct INSERT)
GRANT SELECT ON platform.audit_log TO velocity_api;
GRANT EXECUTE ON FUNCTION platform.audit_insert(
    TEXT, TEXT, TEXT, TEXT, UUID, JSONB, JSONB, TEXT, TEXT, TEXT
) TO velocity_api;
GRANT EXECUTE ON FUNCTION platform.audit_verify_window(TIMESTAMPTZ, TIMESTAMPTZ) TO velocity_api;

-- Future event_log partitions inherit perms via DEFAULT PRIVILEGES.
ALTER DEFAULT PRIVILEGES IN SCHEMA platform
    GRANT SELECT, INSERT ON TABLES TO velocity_api;

-- ─── velocity_operator: owns DDL on platform.* mirror tables ────────────────
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

-- ─── Sequences (BIGSERIAL on outbox tables in tenant schemas) ───────────────
-- Tenant grants happen when the HierarchyOperator provisions a Domain;
-- platform sequences (if any are added later) inherit via default privs.
ALTER DEFAULT PRIVILEGES IN SCHEMA platform
    GRANT USAGE, SELECT ON SEQUENCES TO velocity_api, velocity_operator;

COMMIT;
