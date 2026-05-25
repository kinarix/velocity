//! Reconciler for `velocity.sh/v1/Application`.
//!
//! Stage 3 (ADR-011): materialises an app-scoped `velocity-data-api` Deployment
//! in `{org}-{app}-shared` when `spec.deployment.scope == app` (the default).
//! The pod watches only schemas labelled for this org+app via
//! `VELOCITY_API_LABEL_SELECTOR`, so it serves all non-dedicated domains as a
//! catch-all. Domain-scoped pods (managed by the Domain reconciler) use the
//! longer ingress prefix and win over this pod for their specific domain.
//!
//! Namespace `{org}-{app}-shared` is created here without an owner reference —
//! k8s GC does not cascade across namespaces for namespaced resources. Stage 4
//! adds finalizer-based cleanup.

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::crds::hierarchy::{DeploymentScope, effective_scope};
use velocity_types::crds::{Application, ReconcilePhase};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};
use crate::workload;

const MANAGER: &str = "velocity-operator";

pub async fn reconcile(obj: Arc<Application>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let name = obj.name_any();
    let namespace = obj
        .namespace()
        .ok_or_else(|| ReconcileError::Invalid(format!("Application {name} has no namespace")))?;

    let org = obj.spec.org.clone();
    let app_name = name.clone();

    tracing::info!(%org, %app_name, %namespace, "reconciling Application");

    let shared_ns = format!("{org}-{app_name}-shared");

    // Ensure the shared namespace exists. No owner reference — the Application
    // is in `{org}-platform` and k8s GC won't cascade across namespaces.
    let ns_obj = Namespace {
        metadata: ObjectMeta {
            name: Some(shared_ns.clone()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".into(), MANAGER.into()),
                ("velocity.sh/org".into(), org.clone()),
                ("velocity.sh/app".into(), app_name.clone()),
                ("velocity.sh/scope".into(), "app-shared".into()),
            ])),
            ..Default::default()
        },
        ..Default::default()
    };
    Api::<Namespace>::all(ctx.kube.clone())
        .patch(&shared_ns, &PatchParams::apply(MANAGER).force(), &Patch::Apply(&ns_obj))
        .await
        .map_err(|e| {
            ReconcileError::Invalid(format!("creating shared namespace {shared_ns}: {e}"))
        })?;

    // ADR-011: materialise the app-scoped data-API workload when the effective
    // scope is App. Domain-scoped pods are managed by the Domain reconciler.
    let scope = effective_scope(obj.spec.deployment.as_ref(), None);
    let mut data_api_deployment: Option<String> = None;

    match (scope, ctx.data_api.as_ref()) {
        (DeploymentScope::App, Some(settings)) => {
            let cfg = obj.spec.deployment.as_ref().cloned().unwrap_or_default();
            let synced = workload::sync_app(
                &ctx.kube,
                ctx.provisioner.as_ref(),
                settings,
                &obj,
                &shared_ns,
                &org,
                &app_name,
                &cfg,
                settings.ingress_host.as_deref(),
            )
            .await
            .map_err(|e| {
                ReconcileError::Invalid(format!("app-scoped data-API workload sync: {e}"))
            })?;
            data_api_deployment = Some(synced.deployment_name);
        }
        (DeploymentScope::App, None) => {
            tracing::warn!(
                %shared_ns,
                "Application scope=app but the operator has no \
                 VELOCITY_OPERATOR_DATA_API_IMAGE — no app-scoped data-API created; \
                 traffic will fail until the image is configured"
            );
        }
        (DeploymentScope::Domain, _) => {
            // Every domain creates its own pod via the Domain reconciler.
            tracing::debug!(%org, %app_name, "Application scope=domain — no app-scoped data-API pod");
        }
    }

    let api: Api<Application> = Api::namespaced(ctx.kube.clone(), &namespace);
    let status_patch = json!({
        "status": {
            "phase": ReconcilePhase::Ready,
            "dataApiDeployment": data_api_deployment,
        }
    });
    api.patch_status(&name, &PatchParams::apply(MANAGER), &Patch::Merge(&status_patch))
        .await?;

    tracing::info!(%org, %app_name, "Application ready");

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

pub fn error_policy(_obj: Arc<Application>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "Application reconcile failed");
    error_action(err)
}
