-- 0006_anomaly_alerts.sql
--
-- Phase 6c — audit-driven anomaly detection.
--
-- The operator runs a scheduled scanner that walks `platform.audit_log`
-- for newly-arrived rows and evaluates per-rule heuristics (bulk
-- readers, after-hours writes, repeated denials) against the window.
-- Detections land in `platform.anomaly_alerts` so:
--   * dashboards/UI can browse without re-running the rule queries
--   * dedupe is row-level — a re-detection that's already alerted
--     inside the cooldown window is dropped at INSERT time via the
--     partial unique index below.
--
-- `platform.anomaly_scan_state` is the high-watermark singleton —
-- the scanner only inspects rows with id > last_scanned_id, so a
-- restart resumes exactly where it left off without re-flagging
-- already-evaluated rows.
--
-- Idempotent: safe to re-apply.

BEGIN;

-- High-watermark singleton. The (`last_scanned_occurred_at`,
-- `last_scanned_id`) pair is a composite cursor — `audit_log.id` is a
-- v4 random UUID so it cannot be compared on its own for arrival
-- order. Tuple comparison `(occurred_at, id) > (cursor_ts, cursor_id)`
-- gives a strict, total ordering even if two rows land at the exact
-- same microsecond.
CREATE TABLE IF NOT EXISTS platform.anomaly_scan_state (
    id                          INTEGER     PRIMARY KEY DEFAULT 1,
    last_scanned_occurred_at    TIMESTAMPTZ,
    last_scanned_id             UUID,
    last_scanned_at             TIMESTAMPTZ,
    CONSTRAINT anomaly_scan_state_singleton CHECK (id = 1)
);

-- Idempotent column-add for clusters that already applied the pre-cursor
-- shape of this migration.
ALTER TABLE platform.anomaly_scan_state
    ADD COLUMN IF NOT EXISTS last_scanned_occurred_at TIMESTAMPTZ;

INSERT INTO platform.anomaly_scan_state (id) VALUES (1)
    ON CONFLICT (id) DO NOTHING;

CREATE TABLE IF NOT EXISTS platform.anomaly_alerts (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    detected_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    rule            TEXT        NOT NULL,        -- bulk_reader|after_hours|repeated_denials
    actor           TEXT,                         -- the offending actor, when the rule is per-actor
    schema_org      TEXT,                         -- when the rule fires against a specific schema scope
    severity        TEXT        NOT NULL DEFAULT 'warning',  -- info|warning|critical
    detail          JSONB       NOT NULL,        -- rule-specific structured detail (counts, thresholds, window)
    window_start    TIMESTAMPTZ NOT NULL,
    window_end      TIMESTAMPTZ NOT NULL,
    delivered       BOOLEAN     NOT NULL DEFAULT false,
    delivered_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_anomaly_alerts_detected
    ON platform.anomaly_alerts (detected_at DESC);
CREATE INDEX IF NOT EXISTS idx_anomaly_alerts_actor_rule
    ON platform.anomaly_alerts (actor, rule, detected_at DESC);
CREATE INDEX IF NOT EXISTS idx_anomaly_alerts_undelivered
    ON platform.anomaly_alerts (detected_at)
    WHERE delivered = false;

-- Dedupe: if the same (rule, actor, schema_org) already fired this hour
-- (UTC), drop the new detection. Implemented as a partial unique index
-- over the hour bucket so the constraint naturally renews each hour.
--
-- The cast `detected_at AT TIME ZONE 'UTC'` returns a plain TIMESTAMP
-- (without time zone), and `date_trunc('hour', TIMESTAMP)` is IMMUTABLE
-- — required for unique indexes on expressions. `date_trunc('hour',
-- TIMESTAMPTZ)` is only STABLE because the result depends on the
-- session's TimeZone GUC, so it cannot be indexed directly.
CREATE UNIQUE INDEX IF NOT EXISTS uniq_anomaly_alerts_dedupe
    ON platform.anomaly_alerts (
        rule,
        COALESCE(actor, ''),
        COALESCE(schema_org, ''),
        date_trunc('hour', (detected_at AT TIME ZONE 'UTC'))
    );

-- Grants: operator writes alerts; API server reads them (UI panel).
GRANT SELECT, INSERT, UPDATE ON platform.anomaly_alerts TO velocity_operator;
GRANT SELECT, UPDATE         ON platform.anomaly_scan_state TO velocity_operator;
GRANT SELECT ON platform.anomaly_alerts, platform.anomaly_scan_state TO velocity_api;

COMMIT;
