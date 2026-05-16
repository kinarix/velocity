//! Reconciler for `velocity.sh/v1/Organisation`.
//!
//! Phase 0 scope: there is no Postgres-side provisioning at the Organisation
//! level — the hierarchy CRDs exist so that `Domain` reconciles can resolve
//! their parents and so policy inheritance has a place to live. This
//! reconciler just transitions the resource to Ready.

use std::sync::Arc;

use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::crds::{Organisation, ReconcilePhase};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};

pub async fn reconcile(
    obj: Arc<Organisation>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj
        .namespace()
        .ok_or_else(|| ReconcileError::Invalid(format!("Organisation {name} has no namespace")))?;

    tracing::info!(%name, %namespace, "reconciling Organisation");

    let api: Api<Organisation> = Api::namespaced(ctx.kube.clone(), &namespace);
    let patch = json!({ "status": { "phase": ReconcilePhase::Ready } });
    api.patch_status(&name, &PatchParams::apply("velocity-operator"), &Patch::Merge(&patch))
        .await?;

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

pub fn error_policy(_obj: Arc<Organisation>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "Organisation reconcile failed");
    error_action(err)
}
