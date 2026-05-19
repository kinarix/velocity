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

use serde_json::{json, Value};
use sqlx::{PgPool, Postgres, Transaction};

use crate::auth::AuthDecision;
use crate::identity::Identity;
use crate::registry::ResolvedSchema;

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

/// Write an audit row inside the caller's transaction. `entity_id` is the
/// UUID of the row that was written/updated/deleted; `payload` is the
/// post-image (or, for deletes, just `{ "id": "..." }`).
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
    entity_id: &str,
    payload: &Value,
    decision: Option<&AuthDecision>,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let fail_modes = build_fail_modes(decision);

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
    .bind(payload)
    .bind(fail_modes)
    .bind(request_id)
    .execute(&mut **tx)
    .await?;
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
}
