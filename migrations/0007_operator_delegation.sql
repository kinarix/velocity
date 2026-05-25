-- 0007_operator_delegation.sql
--
-- velocity_operator delegates a narrow set of platform.* privileges to each
-- per-domain role it provisions (see velocity-operator/src/provisioner.rs).
-- For a Postgres GRANT to actually convey rights, the grantor must hold the
-- privilege WITH GRANT OPTION. 0003_grants.sql gave velocity_operator the
-- access it needs for its own DDL, but missed the delegation rights below:
--
--   * platform.event_log         — domain roles INSERT events during writes
--   * platform.audit_insert(...) — domain roles call this for audit rows
--   * platform.idempotency_keys  — API uses it during request handling under
--                                  the per-domain role context
--
-- Without these, `make e2e` (or any first Domain reconcile) fails with:
--   NOTICE:  no privileges were granted for "audit_insert"
--   ERROR:   permission denied for table idempotency_keys
--
-- Idempotent: GRANT ... WITH GRANT OPTION is a no-op when already in place.

BEGIN;

GRANT INSERT ON platform.event_log
    TO velocity_operator WITH GRANT OPTION;

GRANT EXECUTE ON FUNCTION platform.audit_insert(
    TEXT, TEXT, TEXT, TEXT, UUID, JSONB, JSONB, TEXT, TEXT, TEXT
) TO velocity_operator WITH GRANT OPTION;

GRANT SELECT, INSERT, UPDATE ON platform.idempotency_keys
    TO velocity_operator WITH GRANT OPTION;

COMMIT;
