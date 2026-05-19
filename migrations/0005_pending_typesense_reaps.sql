-- 0005_pending_typesense_reaps.sql
--
-- Durable queue for the Phase 5d-3b/c blue-green collection reap.
--
-- After a successful alias flip the operator must eventually drop the
-- old concrete collection — but only after a grace period that lets
-- in-flight queries finish and leaves a manual-rollback window. The
-- original implementation used `tokio::time::sleep` on a detached
-- task; an operator restart during the grace window leaked the old
-- concrete forever (next reconcile saw alias == concrete and
-- declared done). This table backs the work queue: the rebuild
-- inserts a row on flip; a sweeper task polls due rows under
-- FOR UPDATE SKIP LOCKED, deletes the Typesense collection, then
-- deletes the row. Crash-safe — on restart the sweeper rediscovers
-- everything that hasn't expired yet, plus anything past expiry.

BEGIN;

CREATE TABLE IF NOT EXISTS platform.pending_typesense_reaps (
    id            BIGSERIAL PRIMARY KEY,
    -- The concrete collection that the sweeper will delete. Unique
    -- because reaping the same name twice is wasted work; the second
    -- attempt would be a 404 anyway, but the constraint catches the
    -- bug where two rebuilds claim the same source.
    concrete_name TEXT NOT NULL UNIQUE,
    -- For log lines: which alias was flipped away from this concrete.
    alias_name    TEXT NOT NULL,
    -- For log lines: which SchemaDefinition owned the rebuild that
    -- enqueued this row. Plain text rather than FK because the CRD
    -- can disappear (k8s delete) without invalidating the reap.
    schema_uid    TEXT NOT NULL,
    enqueued_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    reap_after    TIMESTAMPTZ NOT NULL
);

-- Sweeper queries "WHERE reap_after <= now()" — keep that hot.
CREATE INDEX IF NOT EXISTS idx_pending_typesense_reaps_due
    ON platform.pending_typesense_reaps (reap_after);

COMMIT;
