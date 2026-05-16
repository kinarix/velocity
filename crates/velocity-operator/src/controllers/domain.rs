//! Reconciler for `velocity.sh/v1/Domain`.

use std::sync::Arc;

use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::crds::{Domain, ReconcilePhase};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};

/// One reconcile pass for a Domain. Idempotent.
pub async fn reconcile(obj: Arc<Domain>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj
        .namespace()
        .ok_or_else(|| ReconcileError::Invalid(format!("Domain {name} has no namespace")))?;

    let org = obj.labels().get("velocity.sh/org").cloned().ok_or_else(|| {
        ReconcileError::Invalid(format!("Domain {namespace}/{name} missing velocity.sh/org label"))
    })?;

    let app = obj.spec.app.clone();
    let domain = name.clone();

    tracing::info!(%org, %app, %domain, %namespace, "reconciling Domain");

    let provisioned = ctx.provisioner.sync_domain(&org, &app, &domain).await?;

    let api: Api<Domain> = Api::namespaced(ctx.kube.clone(), &namespace);
    let status_patch = json!({
        "status": {
            "phase":    ReconcilePhase::Ready,
            "pgSchema": provisioned.pg_schema,
            "pgRoles":  provisioned.pg_roles,
        }
    });
    api.patch_status(&name, &PatchParams::apply("velocity-operator"), &Patch::Merge(&status_patch))
        .await?;

    tracing::info!(%org, %app, %domain, "Domain ready");

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

/// Controller-level error policy.
pub fn error_policy(_obj: Arc<Domain>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "Domain reconcile failed");
    error_action(err)
}
