//! Audit-log writes from the API server.
//!
//! Per ADR-005 the only legal entry point for `platform.audit_log` is the
//! `platform.audit_insert(...)` stored procedure. We thread it inside the
//! same transaction as the data-table write so the audit row commits
//! atomically with the change it describes — no possibility of "the row
//! changed but the audit row didn't, or vice versa".
//!
//! ## Fail-mode payload
//!
//! [`build_fail_modes`] flattens the [`AuthDecision`] the auth middleware
//! attached into the JSONB blob the audit table expects. The strings on
//! the wire come from [`RevocationDecision::as_audit_str`] — *do not* edit
//! them without coordinating with the Grafana dashboards that key off
//! them.
//!
//! ## Action and outcome strings
//!
//! Pinned lowercase to match CLAUDE.md › Metric label cardinality. Used as
//! Grafana / alerting label values, so changes here are breaking for
//! downstream tooling.
//!
//! [`AuthDecision`]: crate::auth::AuthDecision
//! [`RevocationDecision::as_audit_str`]: crate::auth::RevocationDecision::as_audit_str

use serde_json::{json, Map, Value};
use sqlx::{PgPool, Postgres, Transaction};
use velocity_types::crds::schema::Sensitivity;

use crate::auth::AuthDecision;
use crate::identity::Identity;
use crate::registry::ResolvedSchema;

/// Marker the audit row stores in place of a sensitive field's value.
/// Stable across versions — Grafana / SOC tooling greps for it.
pub const REDACTED: &str = "***";

/// Stable action strings the audit table will record. Match the
/// route-level RBAC ops in [`crate::rbac::op`] for cross-referencing.
pub mod action {
    pub const CREATE: &str = "create";
    pub const READ: &str = "read";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const QUERY: &str = "query";
    pub const SEARCH: &str = "search";
}

pub mod outcome {
    pub const SUCCESS: &str = "success";
    #[allow(dead_code)]
    pub const ERROR: &str = "error";
    pub const DENIED: &str = "denied";
}

/// Render the `p_fail_modes` JSONB the audit insert expects.
///
/// Per ADR-003 we *always* record a value — even the happy "Allowed" case
/// — so the audit chain proves the revocation backend was queried. When
/// no auth middleware was wired (Phase 1 fallback / test seam), the blob
/// records `auth: "unwired"` so an operator reading the row can tell the
/// difference between "authenticated and allowed" and "no auth check at
/// all" without re-running anything.
pub fn build_fail_modes(decision: Option<&AuthDecision>) -> Value {
    match decision {
        Some(d) => json!({
            "auth": "verified",
            "revocation": d.revocation.as_audit_str(),
            "revocation_fail_open": d.revocation_fail_open,
            "strategy": d.strategy,
        }),
        None => json!({ "auth": "unwired" }),
    }
}

/// Sensitivity levels that MUST be redacted before the value reaches
/// `platform.audit_log`. Per CLAUDE.md › Security › "Sensitive data
/// never in logs": `financial | pii | confidential` are redacted;
/// `public` and `internal` pass through verbatim.
fn is_sensitive(s: &Sensitivity) -> bool {
    matches!(
        s,
        Sensitivity::Financial | Sensitivity::Pii | Sensitivity::Confidential
    )
}

/// Return a copy of `payload` where any top-level key matching a
/// sensitive field in the schema has its value replaced with [`REDACTED`].
///
/// Keys are preserved so the audit row still proves *which* fields
/// were touched — only the values disappear. Non-object payloads
/// (delete sentinels like `{ "id": "..." }`, RBAC denial code maps)
/// pass through unchanged because they cannot carry user data.
///
/// Nested objects/arrays are not recursed into: the field index is keyed
/// on top-level CRD field names, and JSON-typed fields are themselves
/// the sensitive unit (the whole value gets `***`, not a sub-key walk
/// the registry has no schema for).
pub fn redact_sensitive(schema: &ResolvedSchema, payload: &Value) -> Value {
    let Value::Object(map) = payload else {
        return payload.clone();
    };
    let mut out = Map::with_capacity(map.len());
    for (k, v) in map {
        let redact = schema
            .fields
            .by_name
            .get(k)
            .and_then(|f| f.sensitivity.as_ref())
            .is_some_and(is_sensitive);
        if redact {
            out.insert(k.clone(), Value::String(REDACTED.into()));
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Write an audit row inside the caller's transaction. `entity_id` is the
/// UUID of the row that was written/updated/deleted; `payload` is the
/// post-image (or, for deletes, just `{ "id": "..." }`).
///
/// The payload is passed through [`redact_sensitive`] before reaching
/// the stored procedure — callers may hand over the raw record without
/// risk of leaking PII / financial / confidential field values into
/// `platform.audit_log`.
///
/// Calls `platform.audit_insert()` as `SECURITY DEFINER`, so the
/// per-domain role under `SET LOCAL ROLE` doesn't need direct INSERT
/// privileges on `audit_log`.
#[allow(clippy::too_many_arguments)] // mirrors platform.audit_insert(...) signature
pub async fn write_audit(
    tx: &mut Transaction<'_, Postgres>,
    schema: &ResolvedSchema,
    identity: &Identity,
    action: &str,
    outcome: &str,
    entity_id: Option<&str>,
    payload: &Value,
    decision: Option<&AuthDecision>,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let fail_modes = build_fail_modes(decision);
    let safe_payload = redact_sensitive(schema, payload);

    // entity_id is bound as text and cast in-SQL to uuid; sqlx encodes
    // `None` as NULL, and `NULL::uuid` round-trips as NULL.
    sqlx::query(
        "SELECT platform.audit_insert(
            p_actor       => $1,
            p_action      => $2,
            p_outcome     => $3,
            p_schema_org  => $4,
            p_entity_id   => $5::uuid,
            p_payload     => $6,
            p_fail_modes  => $7,
            p_request_id  => $8,
            p_reason      => NULL,
            p_ticket_ref  => NULL
         )",
    )
    .bind(&identity.actor_id)
    .bind(action)
    .bind(outcome)
    // schema_org is the registry key — same shape audit consumers index on.
    .bind(crate::registry::registry_key(&schema.path))
    .bind(entity_id)
    .bind(safe_payload)
    .bind(fail_modes)
    .bind(request_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Write a success-path audit row in its own short transaction.
///
/// Read handlers (`list`, `get_one`, `query`, `search`) do not naturally
/// share a write-tx with anything — there is no data row mutated, so
/// the "atomic with the change" guarantee that [`write_audit`] provides
/// has no counterpart on the read path. Opening a standalone tx mirrors
/// the [`write_audit_denial`] pattern and keeps the read-path RoleClass
/// (`Reader`) cleanly out of the audit chain — `platform.audit_insert`
/// is `SECURITY DEFINER` and runs as the function owner regardless.
///
/// `entity_id` is `Some(uuid)` for `get_one` (a specific row was read)
/// and `None` for set-shaped reads (`list`, `query`, `search`) where
/// the answer is a count, not a single id.
///
/// **Throughput note (ADR-005, §"audit write throughput is bounded by
/// row-lock contention ~5k/sec"):** wiring audit onto every read makes
/// the read rate the chain's bottleneck. The phases.md acceptance
/// criterion ("Every request → audit entry") wins for now; a future
/// ADR revision will decide whether to sample, batch, or shard the
/// chain.
#[allow(clippy::too_many_arguments)]
pub async fn write_audit_standalone(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    action: &str,
    outcome: &str,
    entity_id: Option<&str>,
    payload: &Value,
    decision: Option<&AuthDecision>,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    write_audit(
        &mut tx,
        schema,
        identity,
        action,
        outcome,
        entity_id,
        payload,
        decision,
        request_id,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Synthetic `schema_org` the platform-internal audit endpoints record
/// against. Pinned so a SOC analyst filtering for `schema_org = '__platform/audit/audit/audit/v1'`
/// gets every "who read the audit log" row in one query. Format
/// matches the org/app/domain/object/version shape so any downstream
/// tooling that parses the path stays happy; the leading `__platform`
/// segment makes it unmistakeably platform-internal and prevents
/// collision with any real tenant org (CRD validation rejects
/// double-underscore prefixes — see `crds/schema.rs::validate_path`).
pub const AUDIT_SELF_SCHEMA_ORG: &str = "__platform/audit/audit/audit/v1";

/// Write a "platform-internal" audit row directly, bypassing
/// [`ResolvedSchema`]-based redaction.
///
/// Used by the audit-read endpoints to self-audit each call: the
/// payload here is the request's filter set + result count (no tenant
/// data), so [`redact_sensitive`] has nothing to do — and there is no
/// `ResolvedSchema` to pass in anyway (the endpoints are not bound to
/// a tenant schema). The `schema_org` is the synthetic
/// [`AUDIT_SELF_SCHEMA_ORG`] string.
///
/// **Never call this from a tenant-data code path.** It deliberately
/// skips redaction; misuse would leak PII into `platform.audit_log`.
#[allow(clippy::too_many_arguments)]
pub async fn write_audit_meta(
    pool: &PgPool,
    actor: &str,
    schema_org: &str,
    action: &str,
    outcome: &str,
    payload: &Value,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Fail-modes is "unwired" here — the platform-audit endpoint uses
    // bearer-token authentication (not the per-schema AuthDecision
    // pipeline), so there's no AuthDecision struct to flatten. Keeping
    // the shape consistent means dashboards don't grow a special case.
    let fail_modes = json!({ "auth": "platform_bearer" });
    sqlx::query(
        "SELECT platform.audit_insert(
            p_actor       => $1,
            p_action      => $2,
            p_outcome     => $3,
            p_schema_org  => $4,
            p_entity_id   => NULL::uuid,
            p_payload     => $5,
            p_fail_modes  => $6,
            p_request_id  => $7,
            p_reason      => NULL,
            p_ticket_ref  => NULL
         )",
    )
    .bind(actor)
    .bind(action)
    .bind(outcome)
    .bind(schema_org)
    .bind(payload)
    .bind(fail_modes)
    .bind(request_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Write a denial audit row in its own short transaction.
///
/// Denials are raised BEFORE the data-write transaction opens (RBAC →
/// ABAC → field-filter all run pre-tx), so they cannot share that tx
/// the way success-path audit does. The trade-off: a denial row is
/// not atomic with anything else — but there is no "anything else" to
/// be atomic with. The 403 happens regardless.
///
/// `entity_id` is always NULL (the row that would have been touched
/// either doesn't exist yet or wasn't reached). `payload` carries the
/// error code so a SOC analyst can pivot on `payload->>'code'` to
/// separate RBAC denials from ABAC, field-filter, and cross-schema
/// denials without re-parsing the action.
#[allow(clippy::too_many_arguments)]
pub async fn write_audit_denial(
    pool: &PgPool,
    schema: &ResolvedSchema,
    identity: &Identity,
    action: &str,
    code: &str,
    decision: Option<&AuthDecision>,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let fail_modes = build_fail_modes(decision);
    let payload = json!({ "code": code });

    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT platform.audit_insert(
            p_actor       => $1,
            p_action      => $2,
            p_outcome     => $3,
            p_schema_org  => $4,
            p_entity_id   => NULL::uuid,
            p_payload     => $5,
            p_fail_modes  => $6,
            p_request_id  => $7,
            p_reason      => NULL,
            p_ticket_ref  => NULL
         )",
    )
    .bind(&identity.actor_id)
    .bind(action)
    .bind(outcome::DENIED)
    .bind(crate::registry::registry_key(&schema.path))
    .bind(payload)
    .bind(fail_modes)
    .bind(request_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthDecision, RevocationDecision};
    use crate::registry::ResolvedSchema;
    use velocity_types::common::SchemaPath;
    use velocity_types::crds::schema::{
        AccessSpec, AuthSpec, FieldKind, FieldSpec, ObservabilitySpec, SchemaDefinitionSpec,
        SearchSpec, SearchTier, Sensitivity,
    };

    fn field_with(name: &str, sensitivity: Option<Sensitivity>) -> FieldSpec {
        let mut f: FieldSpec = serde_json::from_value(serde_json::json!({
            "name": name,
            "type": "string",
        }))
        .unwrap();
        f.kind = FieldKind::String;
        f.sensitivity = sensitivity;
        f
    }

    fn schema_with(fields: Vec<FieldSpec>) -> ResolvedSchema {
        let spec = SchemaDefinitionSpec {
            version: "v1".into(),
            partitioning: None,
            auth: AuthSpec {
                strategy_ref: velocity_types::common::NamespacedRef {
                    name: "default".into(),
                    namespace: "acme-platform".into(),
                },
                overrides: Vec::new(),
            },
            access: AccessSpec::default(),
            fields,
            validations: Vec::new(),
            search: SearchSpec { tier: SearchTier::Tier1, ..Default::default() },
            time_machine: None,
            audit: None,
            archive: None,
            observability: ObservabilitySpec::default(),
            scaling: None,
        };
        let p = SchemaPath::new("acme", "supply-chain", "procurement", "purchase-order", "v1");
        ResolvedSchema::from_spec(p, spec)
    }

    #[test]
    fn fail_modes_none_is_unwired() {
        let v = build_fail_modes(None);
        assert_eq!(v["auth"], "unwired");
        assert!(v.get("revocation").is_none(), "no revocation field when auth is unwired");
    }

    #[test]
    fn fail_modes_allowed_carries_all_audit_fields() {
        // Pinned: Grafana keys off these exact strings — if a refactor
        // renames any, the dashboards stop highlighting backend-down
        // bursts. The test exists to make that breakage loud.
        let d = AuthDecision {
            revocation: RevocationDecision::Allowed,
            revocation_fail_open: false,
            strategy: "acme-platform/default".into(),
        };
        let v = build_fail_modes(Some(&d));
        assert_eq!(v["auth"], "verified");
        assert_eq!(v["revocation"], "allowed");
        assert_eq!(v["revocation_fail_open"], false);
        assert_eq!(v["strategy"], "acme-platform/default");
    }

    #[test]
    fn fail_modes_backend_down_admitted_preserves_open_flag() {
        // ADR-003: an admitted-under-backend-down request must be
        // distinguishable from a normal admit. This is the row a SOC
        // analyst pivots on after a Redis outage.
        let d = AuthDecision {
            revocation: RevocationDecision::BackendDownAdmitted,
            revocation_fail_open: true,
            strategy: "acme-platform/default".into(),
        };
        let v = build_fail_modes(Some(&d));
        assert_eq!(v["revocation"], "backend_down_admitted");
        assert_eq!(v["revocation_fail_open"], true);
    }

    #[test]
    fn redact_sensitive_replaces_pii_financial_confidential_with_marker() {
        // CLAUDE.md › Security: these three classes never reach
        // platform.audit_log in plaintext. The marker must be a literal
        // "***" — SOC dashboards greps for it.
        let schema = schema_with(vec![
            field_with("ssn", Some(Sensitivity::Pii)),
            field_with("salary", Some(Sensitivity::Financial)),
            field_with("contract_terms", Some(Sensitivity::Confidential)),
        ]);
        let payload = json!({
            "ssn": "123-45-6789",
            "salary": 150000,
            "contract_terms": "internal-only",
        });
        let redacted = redact_sensitive(&schema, &payload);
        assert_eq!(redacted["ssn"], REDACTED);
        assert_eq!(redacted["salary"], REDACTED);
        assert_eq!(redacted["contract_terms"], REDACTED);
    }

    #[test]
    fn redact_sensitive_passes_public_internal_through() {
        // Without an explicit redact-list, only sensitive fields are
        // touched. Public + Internal preserve their original JSON shape
        // so audit consumers can still see the data they're allowed to.
        let schema = schema_with(vec![
            field_with("po_number", Some(Sensitivity::Public)),
            field_with("note", Some(Sensitivity::Internal)),
            field_with("untagged", None),
        ]);
        let payload = json!({
            "po_number": "PO-1",
            "note": "ok",
            "untagged": 42,
        });
        let redacted = redact_sensitive(&schema, &payload);
        assert_eq!(redacted["po_number"], "PO-1");
        assert_eq!(redacted["note"], "ok");
        assert_eq!(redacted["untagged"], 42);
    }

    #[test]
    fn redact_sensitive_preserves_keys_for_unknown_fields() {
        // Fields not in the index (e.g. `id`, `created_at`, anything
        // added by the DB) MUST pass through — losing them would mean
        // losing the audit row's whole reason to exist. Only the values
        // of *known sensitive* fields are scrubbed.
        let schema = schema_with(vec![field_with("ssn", Some(Sensitivity::Pii))]);
        let payload = json!({
            "id": "abc",
            "ssn": "123",
            "created_at": "2026-05-19T00:00:00Z",
        });
        let redacted = redact_sensitive(&schema, &payload);
        assert_eq!(redacted["id"], "abc");
        assert_eq!(redacted["ssn"], REDACTED);
        assert_eq!(redacted["created_at"], "2026-05-19T00:00:00Z");
    }

    #[test]
    fn redact_sensitive_non_object_payload_passes_through() {
        // Delete sentinels and denial code-maps aren't always full
        // records — accept any JSON. Non-objects can't carry user data
        // (the schema's field index is keyed on object keys), so
        // there's nothing to scrub.
        let schema = schema_with(vec![field_with("ssn", Some(Sensitivity::Pii))]);
        assert_eq!(redact_sensitive(&schema, &json!(null)), json!(null));
        assert_eq!(redact_sensitive(&schema, &json!("string")), json!("string"));
        assert_eq!(redact_sensitive(&schema, &json!([1, 2, 3])), json!([1, 2, 3]));
    }

    #[test]
    fn redact_sensitive_does_not_recurse_into_nested_objects() {
        // JSON-typed fields are themselves the sensitive unit per the
        // schema — the *whole* JSON blob gets `***`, not a structural
        // walk through keys the registry has no schema for. Document
        // the behaviour so a future refactor doesn't quietly deepen
        // the redaction (which would also be wrong — the registry
        // can't know which sub-keys are sensitive).
        let mut json_field = field_with("metadata", Some(Sensitivity::Pii));
        json_field.kind = FieldKind::Json;
        let schema = schema_with(vec![json_field]);
        let payload = json!({
            "metadata": { "ssn": "123", "harmless": "ok" }
        });
        let redacted = redact_sensitive(&schema, &payload);
        // The whole field becomes `***`; we don't try to redact inside.
        assert_eq!(redacted["metadata"], REDACTED);
    }
}
