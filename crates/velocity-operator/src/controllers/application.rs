//! Reconciler for `velocity.sh/v1/Application`.
//!
//! Phase 0 scope: same as Organisation — no Postgres state change at this
//! level. The reconciler keeps the resource in a Ready state so downstream
//! controllers know the parent is consistent.

use std::sync::Arc;

use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::crds::{Application, ReconcilePhase};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};

pub async fn reconcile(obj: Arc<Application>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj
        .namespace()
        .ok_or_else(|| ReconcileError::Invalid(format!("Application {name} has no namespace")))?;

    tracing::info!(%name, %namespace, app=%obj.spec.org, "reconciling Application");

    let api: Api<Application> = Api::namespaced(ctx.kube.clone(), &namespace);
    let patch = json!({ "status": { "phase": ReconcilePhase::Ready } });
    api.patch_status(&name, &PatchParams::apply("velocity-operator"), &Patch::Merge(&patch))
        .await?;

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

pub fn error_policy(_obj: Arc<Application>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "Application reconcile failed");
    error_action(err)
}
