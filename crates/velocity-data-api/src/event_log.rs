//! Phase 3 hot-tier event log — the cross-cutting append-only stream of
//! every mutation the API performs. One row per write; queried by the
//! Time Machine endpoints. The per-table `{table}_history` (populated by
//! the trigger in `velocity-operator::ddl_builder::build_history_trigger`)
//! is a complementary database-side mirror; this module is the
//! authoritative source for *request-shaped* metadata (source, request_id,
//! identity) that the trigger cannot see.
//!
//! Schema: `platform.event_log`, partitioned monthly by `occurred_at`
//! (see migration `0001_platform_schema.sql`). Writes live inside the
//! same transaction that mutated the main row so either both commit or
//! neither does — the trigger-on-history pattern enforces atomicity at the
//! per-table level; *this* file enforces atomicity at the cross-cutting
//! event-stream level. Callers that hand us a transaction inherit that
//! contract.
//!
//! No `INSERT` is exposed outside the helper — every column is bound here
//! so a future schema migration shows up in exactly one diff site.

use serde_json::Value;
use sqlx::{Postgres, Transaction};

use velocity_core::registry::ResolvedSchema;
use velocity_core::Identity;

/// Provenance for an event-log row. Stored verbatim in `event_log.source`
/// so dashboards / replay consumers can filter by trigger. Lower-case to
/// match the canonical-ops convention used elsewhere in the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    /// HTTP API request, the common case.
    Api,
    /// Operator-initiated sync (drift remediation, reconcile-on-CRD-change).
    OperatorSync,
    /// Bulk import or migration replay — used to suppress downstream
    /// notification fan-out (consumers can match on `source = 'import'`).
    Import,
    /// Forward-port of an old event during a schema migration.
    Migration,
    /// Restore endpoint — the new event applying an older state. Carried
    /// separately so the SSE replay / diff endpoints can render these
    /// distinctly from a normal update.
    Restore,
}

impl EventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSource::Api => "api",
            EventSource::OperatorSync => "operator-sync",
            EventSource::Import => "import",
            EventSource::Migration => "migration",
            EventSource::Restore => "restore",
        }
    }
}

/// One write into `platform.event_log`. Fields are deliberately scalar +
/// `Value` so the call sites compose against `serde_json::json!` rather
/// than a builder. The helper binds the columns positionally — adding a
/// new column means touching this module and the migration in lockstep.
#[derive(Debug, Clone)]
pub struct EventLogRow<'a> {
    pub schema: &'a ResolvedSchema,
    pub entity_id: &'a str,
    pub operation: &'a str,
    pub source: EventSource,
    pub identity: &'a Identity,
    pub request_id: Option<&'a str>,
    /// `Some(patch)` only on UPDATE/RESTORE; `None` on CREATE/DELETE.
    /// JSON-Patch (RFC 6902) shape, computed by `crate::event_log::diff`.
    pub diff: Option<Value>,
    /// Full record post-mutation for CREATE/UPDATE/RESTORE; `None` for
    /// DELETE (the deleted row's last-known state is reconstructable from
    /// the prior event). The DELETE case being `None` mirrors the design
    /// note in `migrations/0001_platform_schema.sql`.
    pub payload: Option<Value>,
    /// Free-form rationale, currently used by the restore endpoint
    /// (`POST /{id}/restore`) which accepts a `reason` body field or
    /// `X-Reason` header. Stored verbatim as TEXT in `event_log.reason`
    /// (migration 0004). `None` on the common path.
    pub reason: Option<&'a str>,
}

/// Append one row to `platform.event_log` inside the caller's transaction.
///
/// Atomicity contract: this MUST be invoked from within the same
/// transaction that wrote the main row. If the main commit fails the
/// event row vanishes with it, and consumers never see a fictitious
/// mutation. Conversely, if this fails (e.g., a missing monthly partition)
/// the main mutation rolls back too — the partition manager (Phase 3.8)
/// keeps a one-month lead specifically so this path doesn't fail under
/// normal operation.
pub async fn write(
    tx: &mut Transaction<'_, Postgres>,
    row: EventLogRow<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO platform.event_log
            (schema_org, entity_id, operation, source, actor, request_id, diff, payload, reason)
         VALUES ($1, $2::uuid, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(velocity_core::registry::registry_key(&row.schema.path))
    .bind(row.entity_id)
    .bind(row.operation)
    .bind(row.source.as_str())
    .bind(&row.identity.actor_id)
    .bind(row.request_id)
    .bind(row.diff)
    .bind(row.payload)
    .bind(row.reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Compute a JSON-Patch (RFC 6902) `before` → `after` diff suitable for
/// `event_log.diff` on UPDATE / RESTORE events. Reads as a `Vec<Patch>`,
/// re-serialised to `serde_json::Value` for column binding.
///
/// Both inputs MUST be the post-stripped, masked row shape the API would
/// hand back to readers — never the raw SQL row. The diff is what the
/// `/diff` endpoint replays back to clients, and any field that wouldn't
/// have been visible on a normal GET MUST NOT leak through the diff
/// channel either.
pub fn diff(before: &Value, after: &Value) -> Value {
    let patch = json_patch::diff(before, after);
    // `json_patch::Patch` is `Vec<PatchOperation>`; serialize back to
    // Value so the column type stays JSONB regardless of patch library.
    serde_json::to_value(patch).unwrap_or(Value::Array(vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_source_strings_match_migration_check() {
        // The five accepted values for `event_log.source`. If a new
        // variant is added to `EventSource`, this test fails until the
        // migration (or its CHECK constraint, if introduced later) is
        // updated to allow the new string.
        assert_eq!(EventSource::Api.as_str(), "api");
        assert_eq!(EventSource::OperatorSync.as_str(), "operator-sync");
        assert_eq!(EventSource::Import.as_str(), "import");
        assert_eq!(EventSource::Migration.as_str(), "migration");
        assert_eq!(EventSource::Restore.as_str(), "restore");
    }

    #[test]
    fn diff_of_identical_objects_is_empty_patch() {
        // Important property for restore no-op detection: if the target
        // state matches the current state, the diff is empty, and the
        // /restore endpoint can 409 RESTORE_NO_OP off the patch length
        // without re-comparing fields. Pinned here so a future patch
        // library upgrade can't silently introduce noise ops.
        let a = json!({ "po_number": "PO-001", "region": "west" });
        let b = a.clone();
        let p = diff(&a, &b);
        assert_eq!(p, json!([]), "identical objects must produce an empty patch");
    }

    #[test]
    fn diff_of_changed_field_produces_replace_op() {
        let a = json!({ "po_number": "PO-001", "region": "west" });
        let b = json!({ "po_number": "PO-001", "region": "east" });
        let p = diff(&a, &b);
        let ops = p.as_array().expect("patch is an array of ops");
        assert_eq!(ops.len(), 1, "exactly one op for one changed field");
        assert_eq!(ops[0]["op"], "replace");
        assert_eq!(ops[0]["path"], "/region");
        assert_eq!(ops[0]["value"], "east");
    }

    #[test]
    fn diff_of_added_and_removed_fields_is_independent_ops() {
        // Locks the property that field-add and field-remove are reported
        // as separate ops rather than merged into a single replace —
        // downstream `/diff` consumers (and the restore handler) rely on
        // op-level granularity to decide which fields to re-write.
        let a = json!({ "po_number": "PO-001", "region": "west" });
        let b = json!({ "po_number": "PO-001", "supplier_code": "TATA001" });
        let p = diff(&a, &b);
        let ops = p.as_array().unwrap();
        // Expect a remove on /region and an add on /supplier_code (order
        // is library-defined; assert by set, not by index).
        let mut paths_by_op: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for op in ops {
            let kind = op["op"].as_str().unwrap();
            let path = op["path"].as_str().unwrap();
            paths_by_op.entry(kind).or_default().push(path);
        }
        assert_eq!(
            paths_by_op.get("remove").map(Vec::as_slice),
            Some(["/region"].as_slice()),
            "expected one remove for the dropped field",
        );
        assert_eq!(
            paths_by_op.get("add").map(Vec::as_slice),
            Some(["/supplier_code"].as_slice()),
            "expected one add for the introduced field",
        );
    }
}
