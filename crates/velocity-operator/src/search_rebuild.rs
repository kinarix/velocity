//! Phase 5d-3b — Tier-3 Typesense blue-green rebuild.
//!
//! When a SchemaDefinition spec changes in a way that produces a new
//! concrete Typesense collection (see
//! [`schema_concrete_collection_name`](velocity_typesense::schema_concrete_collection_name)),
//! the reconciler:
//!
//! 1. Creates the new concrete collection (5d-3a `concrete_collection_spec`).
//! 2. Spawns a background rebuild task ([`run`]).
//! 3. Returns immediately so the reconcile doesn't block on the
//!    backfill — kube-runtime is free to do other work.
//!
//! The task itself:
//!
//! - Takes a paginated snapshot of the main Postgres table, building
//!   the same Typesense doc the CDC worker writes
//!   ([`velocity_typesense::build_doc`]) and upserting it into the
//!   *new concrete* collection (not the alias).
//! - Performs a delta pass for rows that changed during the snapshot.
//!   The pass is cheap (`WHERE updated_at >= started_at`) and runs in
//!   a small loop until the unseen-row count drops below a threshold.
//! - Calls `upsert_alias(alias, new_concrete)` — the atomic flip.
//! - Schedules deletion of the old concrete after a grace period so
//!   any in-flight queries on the old target can finish cleanly.
//!
//! ## Failure semantics (ADR-003)
//!
//! Anything that errors here surfaces in the SchemaDefinition status
//! and increments `velocity_search_rebuild_failures_total`. The alias
//! stays put — search keeps working at old freshness. A subsequent
//! reconcile (kube-runtime requeue, or operator restart) re-detects
//! the mismatch and respawns the rebuild. The concrete collection is
//! deterministic from the spec, so re-scanning is idempotent.
//!
//! ## Race window
//!
//! Snapshot-and-flip leaves a tight window between the last delta
//! pass and the alias flip during which a write goes to old concrete
//! but is invisible on the new one. The window self-heals on the
//! next write to that row (CDC upserts the row into whatever the
//! alias points at — now the new concrete). For real-time
//! consistency requirements the follow-up dual-write design needs to
//! land; not in 5d-3b scope per advisor.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kube::api::{Api, Patch, PatchParams};
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use velocity_types::common::SchemaPath;
use velocity_types::crds::{ReconcilePhase, SchemaDefinition};
use velocity_typesense::{build_doc, TypesenseClient, TypesenseError};

/// Default page size for the snapshot scan. Conservative — backfill
/// is bounded by Typesense's single-doc upsert throughput, not
/// Postgres.
const PAGE_SIZE: i64 = 200;
/// Maximum delta-pass iterations before we declare the source quiet
/// and flip the alias. Each iteration is bounded by `PAGE_SIZE * N
/// rows`. In practice steady-state systems converge in 1-2 passes.
const MAX_DELTA_PASSES: u32 = 5;
/// Grace period before the old concrete collection is reaped after a
/// successful alias flip. Lets in-flight queries finish cleanly and
/// leaves a short manual-rollback window via `upsert_alias`.
const REAP_GRACE: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("typesense: {0}")]
    Typesense(#[from] TypesenseError),
    #[error("kube: {0}")]
    Kube(#[from] kube::Error),
    #[error("cancelled")]
    Cancelled,
}

/// Arguments handed to the spawned task. Cheap to construct on every
/// reconcile because we don't keep one alive across reconciles.
#[allow(missing_debug_implementations)]
pub struct RebuildArgs {
    pub kube: kube::Client,
    pub pool: PgPool,
    pub typesense: TypesenseClient,
    pub namespace: String,
    pub crd_name: String,
    pub path: SchemaPath,
    pub pg_schema: String,
    pub pg_table: String,
    pub alias: String,
    pub source_concrete: String,
    pub target_concrete: String,
    pub cancel: CancellationToken,
}

/// Run the rebuild end-to-end. Caller has already created the target
/// concrete collection in Typesense; this only handles backfill +
/// delta + flip + reap.
pub async fn run(args: RebuildArgs) -> Result<u64, RebuildError> {
    let started_at = Utc::now();
    patch_status(
        &args,
        Some(ReconcilePhase::Rebuilding),
        json!({
            "targetConcrete": args.target_concrete,
            "sourceConcrete": args.source_concrete,
            "startedAt": started_at.to_rfc3339(),
            "rowsProcessed": 0u64,
        }),
    )
    .await?;

    // ── Snapshot pass: paginate by id. We use id (UUID/text) keyset
    // pagination rather than OFFSET to keep the cost flat as the
    // table grows.
    let total_table = format!("\"{}\".\"{}\"", args.pg_schema, args.pg_table);
    let mut cursor: Option<String> = None;
    let mut rows_processed: u64 = 0;
    loop {
        if args.cancel.is_cancelled() {
            return Err(RebuildError::Cancelled);
        }
        let batch = fetch_page(&args.pool, &total_table, cursor.as_deref()).await?;
        if batch.is_empty() {
            break;
        }
        for (id, payload) in &batch {
            if args.cancel.is_cancelled() {
                return Err(RebuildError::Cancelled);
            }
            let doc = build_doc(&args.path, id, Some(payload));
            args.typesense.upsert(&args.target_concrete, &doc).await?;
            rows_processed += 1;
        }
        let last_id = batch.last().map(|(id, _)| id.clone());
        cursor = last_id;
        if rows_processed.is_multiple_of(1000) {
            patch_status(
                &args,
                Some(ReconcilePhase::Rebuilding),
                json!({
                    "targetConcrete": args.target_concrete,
                    "sourceConcrete": args.source_concrete,
                    "startedAt": started_at.to_rfc3339(),
                    "rowsProcessed": rows_processed,
                }),
            )
            .await?;
        }
        if batch.len() < PAGE_SIZE as usize {
            break;
        }
    }
    info!(
        rows = rows_processed,
        target = %args.target_concrete,
        "snapshot pass complete"
    );

    // ── Delta passes: any row whose updated_at moved past
    // `started_at` while we were scanning gets re-upserted. Bounded
    // by MAX_DELTA_PASSES so we don't loop forever under a constant
    // write stream — the next reconcile will pick up the slack via
    // the standard alias-flip race-window self-heal.
    let mut delta_cutoff = started_at;
    for pass in 0..MAX_DELTA_PASSES {
        if args.cancel.is_cancelled() {
            return Err(RebuildError::Cancelled);
        }
        let next_cutoff = Utc::now();
        let delta_rows =
            fetch_delta(&args.pool, &total_table, delta_cutoff.to_rfc3339()).await?;
        if delta_rows.is_empty() {
            info!(pass, "delta pass empty; converged");
            break;
        }
        info!(pass, delta = delta_rows.len(), "delta pass");
        for (id, payload) in &delta_rows {
            let doc = build_doc(&args.path, id, Some(payload));
            args.typesense.upsert(&args.target_concrete, &doc).await?;
            rows_processed += 1;
        }
        delta_cutoff = next_cutoff;
    }

    // ── Flip the alias (atomic in Typesense; PUT /aliases/<name>).
    info!(
        alias = %args.alias,
        from = %args.source_concrete,
        to = %args.target_concrete,
        "flipping Typesense alias"
    );
    args.typesense.upsert_alias(&args.alias, &args.target_concrete).await?;

    let finished_at = Utc::now();
    patch_status(
        &args,
        Some(ReconcilePhase::Ready),
        json!({
            "targetConcrete": args.target_concrete,
            "sourceConcrete": args.source_concrete,
            "startedAt": started_at.to_rfc3339(),
            "finishedAt": finished_at.to_rfc3339(),
            "rowsProcessed": rows_processed,
            "lastDeltaAt": delta_cutoff.to_rfc3339(),
        }),
    )
    .await?;

    // ── Reap the old concrete after a grace period. Detached
    // task — if the operator is killed during this window the
    // collection lingers; the next reconcile will note it as a
    // drift candidate (Phase 5d-3c is the only place that would
    // sweep it).
    let ts = args.typesense.clone();
    let to_drop = args.source_concrete.clone();
    let alias_for_log = args.alias.clone();
    tokio::spawn(async move {
        tokio::time::sleep(REAP_GRACE).await;
        if let Err(e) = ts.delete_collection(&to_drop).await {
            warn!(
                alias = %alias_for_log,
                old = %to_drop,
                error = %e,
                "failed to delete old Typesense collection after grace period"
            );
        } else {
            info!(alias = %alias_for_log, old = %to_drop, "reaped old Typesense collection");
        }
    });

    Ok(rows_processed)
}

async fn fetch_page(
    pool: &PgPool,
    table: &str,
    cursor: Option<&str>,
) -> Result<Vec<(String, Value)>, sqlx::Error> {
    let sql = match cursor {
        None => format!(
            "SELECT id::text AS id, to_jsonb(t) - '__fts' AS payload \
             FROM {table} t \
             WHERE deleted_at IS NULL \
             ORDER BY id ASC \
             LIMIT {PAGE_SIZE}"
        ),
        Some(_) => format!(
            "SELECT id::text AS id, to_jsonb(t) - '__fts' AS payload \
             FROM {table} t \
             WHERE deleted_at IS NULL AND id::text > $1 \
             ORDER BY id ASC \
             LIMIT {PAGE_SIZE}"
        ),
    };
    let mut q = sqlx::query_as::<_, (String, Value)>(&sql);
    if let Some(c) = cursor {
        q = q.bind(c);
    }
    q.fetch_all(pool).await
}

async fn fetch_delta(
    pool: &PgPool,
    table: &str,
    cutoff_rfc3339: String,
) -> Result<Vec<(String, Value)>, sqlx::Error> {
    let sql = format!(
        "SELECT id::text AS id, to_jsonb(t) - '__fts' AS payload \
         FROM {table} t \
         WHERE deleted_at IS NULL AND updated_at >= $1::timestamptz \
         ORDER BY id ASC \
         LIMIT 5000"
    );
    sqlx::query_as::<_, (String, Value)>(&sql)
        .bind(cutoff_rfc3339)
        .fetch_all(pool)
        .await
}

async fn patch_status(
    args: &RebuildArgs,
    phase: Option<ReconcilePhase>,
    search_rebuild: Value,
) -> Result<(), RebuildError> {
    let api: Api<SchemaDefinition> = Api::namespaced(args.kube.clone(), &args.namespace);
    let body = if let Some(p) = phase {
        json!({ "status": { "phase": p, "searchRebuild": search_rebuild } })
    } else {
        json!({ "status": { "searchRebuild": search_rebuild } })
    };
    api.patch_status(
        &args.crd_name,
        &PatchParams::apply("velocity-operator-rebuild"),
        &Patch::Merge(&body),
    )
    .await?;
    Ok(())
}

/// Reusable lookup table of in-flight rebuilds. Lives in
/// `Context`; the reconciler reads / updates it.
#[derive(Debug, Default)]
pub struct RebuildRegistry {
    inner: dashmap::DashMap<String, RebuildHandle>,
}

#[derive(Debug)]
struct RebuildHandle {
    target_concrete: String,
    cancel: CancellationToken,
    _join: Arc<tokio::task::JoinHandle<()>>,
}

impl RebuildRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel any in-flight rebuild whose target concrete name doesn't
    /// match `desired_target`, then return whether a new rebuild
    /// should be spawned (`true`) or one matching `desired_target` is
    /// already running (`false`).
    pub fn supersede(&self, uid: &str, desired_target: &str) -> bool {
        if let Some(existing) = self.inner.get(uid) {
            if existing.target_concrete == desired_target {
                return false;
            }
            existing.cancel.cancel();
        }
        true
    }

    pub fn record(
        &self,
        uid: String,
        target_concrete: String,
        cancel: CancellationToken,
        join: tokio::task::JoinHandle<()>,
    ) {
        self.inner.insert(
            uid,
            RebuildHandle {
                target_concrete,
                cancel,
                _join: Arc::new(join),
            },
        );
    }

    pub fn forget(&self, uid: &str) {
        self.inner.remove(uid);
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_supersedes_on_target_change() {
        let r = RebuildRegistry::new();
        let cancel = CancellationToken::new();
        let join = tokio::spawn(async {});
        r.record("uid-1".into(), "alias__aaaa1111".into(), cancel.clone(), join);

        // Same target → no new spawn.
        assert!(!r.supersede("uid-1", "alias__aaaa1111"));
        assert!(!cancel.is_cancelled(), "same-target supersede must not cancel");

        // Different target → previous is cancelled, caller should spawn.
        assert!(r.supersede("uid-1", "alias__bbbb2222"));
        assert!(cancel.is_cancelled(), "stale target must be cancelled");
    }

    #[test]
    fn registry_supersede_on_unknown_uid_returns_true() {
        let r = RebuildRegistry::new();
        assert!(r.supersede("never-seen", "alias__deadbeef"));
        assert!(r.is_empty());
    }
}
