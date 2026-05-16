//! Reconciler for `velocity.sh/v1/SchemaDefinition`.
//!
//! Phase 1 scope: given a SchemaDefinition CRD, build the [`DdlPlan`] and apply
//! it via the provisioner — creating the main, history, and (Tier-3) outbox
//! tables along with auto-generated indexes and triggers.
//!
//! Breaking schema changes (DropColumn / ChangeType / etc.) are rejected
//! unless the CRD carries `velocity.sh/breaking-change: approved` in its
//! annotations (CLAUDE.md › Blocking breaking changes).

use std::sync::Arc;

use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use velocity_types::common::SchemaPath;
use velocity_types::crds::{ReconcilePhase, SchemaDefinition};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};
use crate::ddl_builder::build_ddl;

const BREAKING_CHANGE_ANN: &str = "velocity.sh/breaking-change";

pub async fn reconcile(
    obj: Arc<SchemaDefinition>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj.namespace().ok_or_else(|| {
        ReconcileError::Invalid(format!("SchemaDefinition {name} has no namespace"))
    })?;

    let org = obj.labels().get("velocity.sh/org").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!(
            "SchemaDefinition {namespace}/{name} missing velocity.sh/org label"
        ))
    })?;
    let app = obj.labels().get("velocity.sh/app").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!(
            "SchemaDefinition {namespace}/{name} missing velocity.sh/app label"
        ))
    })?;
    let domain = obj.labels().get("velocity.sh/domain").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!(
            "SchemaDefinition {namespace}/{name} missing velocity.sh/domain label"
        ))
    })?;

    let path = SchemaPath::new(&org, &app, &domain, &name, &obj.spec.version);
    let allow_breaking = obj
        .annotations()
        .get(BREAKING_CHANGE_ANN)
        .is_some_and(|v| v.eq_ignore_ascii_case("approved"));

    tracing::info!(
        %org, %app, %domain, object = %name, version = %obj.spec.version, %namespace,
        allow_breaking,
        "reconciling SchemaDefinition"
    );

    // Skip-if-unchanged. The hash includes the breaking-change annotation so
    // that toggling it forces a re-evaluation.
    let hash = hash_spec(&obj, allow_breaking);
    let uid = obj.uid().unwrap_or_default();
    if let Some(prev) = ctx.last_hash.get(&uid) {
        if *prev == hash {
            tracing::debug!(uid, "no-op reconcile (hash unchanged)");
            return Ok(Action::requeue(std::time::Duration::from_secs(300)));
        }
    }

    let plan = build_ddl(&obj.spec, &path).map_err(|e| ReconcileError::Invalid(e.to_string()))?;
    let provisioned = ctx.provisioner.sync_schema_tables(&plan, allow_breaking).await?;

    let api: Api<SchemaDefinition> = Api::namespaced(ctx.kube.clone(), &namespace);
    let status_patch = json!({
        "status": {
            "phase": ReconcilePhase::Ready,
            "pgTable": provisioned.qualified,
            "policyHash": hash,
            "provisionedAt": chrono::Utc::now().to_rfc3339(),
        }
    });
    api.patch_status(&name, &PatchParams::apply("velocity-operator"), &Patch::Merge(&status_patch))
        .await?;

    ctx.last_hash.insert(uid, hash);
    tracing::info!(object = %name, qualified = %provisioned.qualified, "SchemaDefinition ready");

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

pub fn error_policy(
    _obj: Arc<SchemaDefinition>,
    err: &ReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    tracing::warn!(error = %err, "SchemaDefinition reconcile failed");
    error_action(err)
}

/// Stable hash over spec + the bits of metadata that affect reconcile output.
/// `serde_json::to_vec` is deterministic on our types (no maps with unstable
/// iteration order on the hot path — BTreeMap is used everywhere).
fn hash_spec(obj: &SchemaDefinition, allow_breaking: bool) -> String {
    let mut h = Sha256::new();
    if let Ok(bytes) = serde_json::to_vec(&obj.spec) {
        h.update(bytes);
    }
    h.update([u8::from(allow_breaking)]);
    format!("{:x}", h.finalize())
}
