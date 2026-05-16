-- 0002_audit_insert.sql
--
-- ADR-005 — audit chain construction via DB stored procedure.
--
-- Application code MUST call platform.audit_insert(...) — direct INSERTs into
-- platform.audit_log are revoked from PUBLIC and from velocity_api in 0003.

BEGIN;

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

COMMENT ON FUNCTION platform.audit_insert IS
    'ADR-005 audit chain writer. SECURITY DEFINER — only entry point for audit_log writes.';

-- Verification helper: walk the chain, recompute each hash, surface tampered rows.
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

COMMENT ON FUNCTION platform.audit_verify_window IS
    'Recompute each row hash for a window; rows where stored != computed are tampered.';

COMMIT;
