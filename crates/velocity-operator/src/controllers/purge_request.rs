//! Reconciler for `velocity.sh/v1/PurgeRequest`.
//!
//! Phase 8 slice 7: manual hard-delete of archived rows from the
//! per-domain `*_archive` schema, gated by a human-applied
//! `velocity.sh/approved-by` annotation.
//!
//! Lifecycle:
//!
//! 1. Spec validation. The `(schema, version)` pair must form a sane
//!    identifier; `olderThan` must parse as RFC 3339.
//! 2. Approval gate. Until the request carries
//!    `velocity.sh/approved-by` annotation, the controller idles in
//!    `Pending` with an `Approved=False` condition.
//! 3. Hard delete. Once approved, the controller deletes every row in
//!    `<org>_<app>_<domain>_archive.<schema>_<version>` whose
//!    `archived_at < olderThan`. Single statement, single transaction.
//! 4. Status writes: `phase=Ready`, `approved=true`, `approvedBy`,
//!    `purgedAt`, `purgedRecords`.
//!
//! Idempotent: a re-applied PurgeRequest with the same spec re-runs the
//! DELETE, which is a no-op once the prior run already drained the
//! matching window.

use std::sync::Arc;

use chrono::Utc;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::common::sanitize;
use velocity_types::crds::{Condition, PurgeRequest, ReconcilePhase};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};

const MANAGER: &str = "velocity-operator";
const APPROVED_BY_ANNOTATION: &str = "velocity.sh/approved-by";

pub async fn reconcile(
    obj: Arc<PurgeRequest>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj.namespace().ok_or_else(|| {
        ReconcileError::Invalid(format!("PurgeRequest {name} has no namespace"))
    })?;

    tracing::info!(
        %name, %namespace,
        schema = %obj.spec.schema, version = %obj.spec.version,
        older_than = %obj.spec.older_than,
        "reconciling PurgeRequest"
    );

    let mut conditions = validate_spec(&obj);

    let approved_by = obj.annotations().get(APPROVED_BY_ANNOTATION).cloned();
    let approved = approved_by.is_some();
    conditions.push(check(
        "Approved",
        if approved {
            Ok(())
        } else {
            Err(format!(
                "set annotation {APPROVED_BY_ANNOTATION} to a human identifier to approve"
            ))
        },
    ));

    let spec_valid = conditions
        .iter()
        .filter(|c| c.kind != "Approved")
        .all(|c| c.status == "True");

    let mut purged_records: Option<u64> = obj
        .status
        .as_ref()
        .and_then(|s| s.purged_records);
    let mut purged_at: Option<String> = obj.status.as_ref().and_then(|s| s.purged_at.clone());

    if spec_valid && approved {
        match resolve_path_parts(&obj) {
            Ok((org, app, domain)) => {
                let pg_schema =
                    format!("{}_{}_{}_archive", sanitize(&org), sanitize(&app), sanitize(&domain));
                let table = format!(
                    "{}_{}",
                    sanitize(&obj.spec.schema),
                    sanitize(&obj.spec.version)
                );
                match purge_archived(&ctx, &pg_schema, &table, &obj.spec.older_than).await {
                    Ok(n) => {
                        purged_records = Some(n);
                        purged_at = Some(Utc::now().to_rfc3339());
                        conditions.push(check("Purged", Ok(())));
                    }
                    Err(msg) => {
                        conditions.push(check("Purged", Err(msg)));
                    }
                }
            }
            Err(msg) => {
                conditions.push(check("Purged", Err(msg)));
            }
        }
    }

    let phase = if conditions.iter().all(|c| c.status == "True") {
        ReconcilePhase::Ready
    } else if !approved {
        ReconcilePhase::Pending
    } else {
        ReconcilePhase::Failed
    };

    let api: Api<PurgeRequest> = Api::namespaced(ctx.kube.clone(), &namespace);
    let mut status = json!({
        "phase": phase,
        "approved": approved,
        "conditions": conditions,
    });
    if let Some(by) = approved_by {
        status["approvedBy"] = json!(by);
    }
    if let Some(n) = purged_records {
        status["purgedRecords"] = json!(n);
    }
    if let Some(ts) = purged_at {
        status["purgedAt"] = json!(ts);
    }
    let patch = json!({ "status": status });
    api.patch_status(&name, &PatchParams::apply(MANAGER), &Patch::Merge(&patch))
        .await?;

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

pub fn error_policy(_obj: Arc<PurgeRequest>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "PurgeRequest reconcile failed");
    error_action(err)
}

// ─── Validation ────────────────────────────────────────────────────────────

fn validate_spec(obj: &PurgeRequest) -> Vec<Condition> {
    let mut out = Vec::new();
    out.push(check("SchemaValid", validate_ident_str(&obj.spec.schema, "schema")));
    out.push(check("VersionValid", validate_ident_str(&obj.spec.version, "version")));
    out.push(check(
        "OlderThanValid",
        chrono::DateTime::parse_from_rfc3339(&obj.spec.older_than)
            .map(|_| ())
            .map_err(|e| format!("olderThan {:?}: {e}", obj.spec.older_than)),
    ));
    out
}

fn validate_ident_str(s: &str, label: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{label} is empty"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!(
            "{label} {s:?} contains characters other than [a-zA-Z0-9_.-]"
        ));
    }
    Ok(())
}

fn check(kind: &str, res: Result<(), String>) -> Condition {
    match res {
        Ok(()) => Condition {
            kind: kind.into(),
            status: "True".into(),
            reason: Some("Ok".into()),
            message: None,
            last_transition_time: None,
        },
        Err(msg) => Condition {
            kind: kind.into(),
            status: "False".into(),
            reason: Some("NotReady".into()),
            message: Some(msg),
            last_transition_time: None,
        },
    }
}

fn resolve_path_parts(obj: &PurgeRequest) -> Result<(String, String, String), String> {
    let labels = obj.labels();
    let org = labels
        .get("velocity.sh/org")
        .ok_or_else(|| "PurgeRequest missing label velocity.sh/org".to_string())?
        .clone();
    let app = labels
        .get("velocity.sh/app")
        .ok_or_else(|| "PurgeRequest missing label velocity.sh/app".to_string())?
        .clone();
    let domain = labels
        .get("velocity.sh/domain")
        .ok_or_else(|| "PurgeRequest missing label velocity.sh/domain".to_string())?
        .clone();
    Ok((org, app, domain))
}

async fn purge_archived(
    ctx: &Context,
    pg_schema: &str,
    table: &str,
    older_than: &str,
) -> Result<u64, String> {
    if !is_safe_ident(pg_schema) || !is_safe_ident(table) {
        return Err(format!(
            "refusing unsafe identifier: schema={pg_schema:?} table={table:?}"
        ));
    }
    let sql = format!(
        "WITH deleted AS (
    DELETE FROM {pg_schema}.{table}
    WHERE archived_at < $1::timestamptz
    RETURNING id
)
SELECT count(*)::bigint FROM deleted;"
    );
    let count: i64 = sqlx::query_scalar(&sql)
        .bind(older_than)
        .fetch_one(&ctx.pg)
        .await
        .map_err(|e| format!("purge sql: {e}"))?;
    Ok(count as u64)
}

fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ident_accepts_normal_names() {
        assert!(validate_ident_str("purchase-order", "schema").is_ok());
        assert!(validate_ident_str("v1", "version").is_ok());
        assert!(validate_ident_str("po.v2", "schema").is_ok());
    }

    #[test]
    fn validate_ident_rejects_unsafe_chars() {
        assert!(validate_ident_str("", "schema").is_err());
        assert!(validate_ident_str("a b", "schema").is_err());
        assert!(validate_ident_str("a;drop", "schema").is_err());
    }

    #[test]
    fn validate_older_than_requires_rfc3339() {
        use velocity_types::crds::purge::PurgeRequestSpec;
        let req = PurgeRequest::new(
            "po-purge",
            PurgeRequestSpec {
                schema: "purchase-order".into(),
                version: "v1".into(),
                older_than: "yesterday".into(),
                estimated_records: None,
                reason: None,
            },
        );
        let conds = validate_spec(&req);
        assert_eq!(
            conds.iter().find(|c| c.kind == "OlderThanValid").unwrap().status,
            "False"
        );
    }

    #[test]
    fn validate_happy_path() {
        use velocity_types::crds::purge::PurgeRequestSpec;
        let req = PurgeRequest::new(
            "po-purge",
            PurgeRequestSpec {
                schema: "purchase-order".into(),
                version: "v1".into(),
                older_than: "2025-01-01T00:00:00Z".into(),
                estimated_records: Some(1000),
                reason: Some("retention".into()),
            },
        );
        let conds = validate_spec(&req);
        assert!(conds.iter().all(|c| c.status == "True"), "{conds:?}");
    }

    #[test]
    fn is_safe_ident_blocks_injection() {
        assert!(is_safe_ident("acme_sc_proc_archive"));
        assert!(is_safe_ident("purchase_order_v1"));
        assert!(!is_safe_ident("acme; drop table x"));
        assert!(!is_safe_ident("1bad"));
        assert!(!is_safe_ident(""));
        assert!(!is_safe_ident(&"x".repeat(64)));
    }
}
