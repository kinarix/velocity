-- Phase 3.5 — add `reason` to platform.event_log.
-- The restore endpoint accepts a `?reason=` / X-Reason header and stores
-- it alongside the event so audit replays can see the rationale text
-- exactly as the operator entered it. Stored as plain TEXT (not JSONB)
-- because it's a single free-form string; nothing else queries against it.
-- Nullable because non-restore events typically have nothing to say here.

ALTER TABLE platform.event_log
    ADD COLUMN IF NOT EXISTS reason TEXT;

-- No new index. Reason is read-back only — never a query predicate — so
-- adding an index would just cost write bandwidth on every mutation.
