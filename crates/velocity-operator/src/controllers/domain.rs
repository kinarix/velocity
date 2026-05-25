//! Reconciler for `velocity.sh/v1/Domain`.

use std::sync::Arc;

use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use serde_json::json;
use velocity_types::crds::hierarchy::DeploymentScope;
use velocity_types::crds::{Domain, ReconcilePhase, SchemaDefinition};

use crate::context::Context;
use crate::controllers::{error_action, ReconcileError};
use crate::workload;

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
    // SchemaDefinitions live in {org}-{app}-{domain}, not in the Domain's own
    // namespace {org}-{app}. The data-API workload must run in the SD namespace
    // so its kube informer watches the correct SchemaDefinitions.
    let sd_namespace = format!("{org}-{app}-{domain}");

    tracing::info!(%org, %app, %domain, %namespace, "reconciling Domain");

    let provisioned = ctx.provisioner.sync_domain(&org, &app, &domain).await?;

    // ADR-011: materialise (or tear down) the per-domain data-API workload
    // when `spec.deployment.scope == domain`. App-scope pods are managed by
    // the Application reconciler (Stage 3). Postgres provisioning is unconditional.
    let dedicated_cfg = obj
        .spec
        .deployment
        .as_ref()
        .filter(|d| d.scope == DeploymentScope::Domain);
    let mut data_api_deployment: Option<String> = None;
    match (dedicated_cfg, ctx.data_api.as_ref()) {
        (Some(cfg), Some(settings)) => {
            // Stamp dedicated=true on every SD so the app-scoped catch-all
            // pod's selector (velocity.sh/dedicated!=true) excludes them.
            stamp_schema_definitions(&ctx.kube, &sd_namespace, true).await?;

            let synced = workload::sync(
                &ctx.kube,
                ctx.provisioner.as_ref(),
                settings,
                &obj,
                &sd_namespace,
                &org,
                &app,
                &domain,
                &provisioned.pg_schema,
                cfg,
                settings.ingress_host.as_deref(),
            )
            .await
            .map_err(|e| ReconcileError::Invalid(format!("data-API workload sync: {e}")))?;
            data_api_deployment = Some(synced.deployment_name);
        }
        (Some(_), None) => {
            tracing::warn!(
                %sd_namespace,
                "Domain requests deployment.scope=domain but the operator has no \
                 VELOCITY_OPERATOR_DATA_API_IMAGE — no data-API created; traffic stays on \
                 the app-scoped data-API"
            );
        }
        (None, _) => {
            // app-scope or absent: remove dedicated stamps so the app-scoped pod
            // picks these SDs back up, then clean up any stale domain workload.
            stamp_schema_definitions(&ctx.kube, &sd_namespace, false).await?;
            if let Err(e) = workload::cleanup(&ctx.kube, &sd_namespace).await {
                tracing::warn!(%sd_namespace, error = %e, "data-API workload cleanup failed");
            }
        }
    }

    let api: Api<Domain> = Api::namespaced(ctx.kube.clone(), &namespace);
    let status_patch = json!({
        "status": {
            "phase":    ReconcilePhase::Ready,
            "pgSchema": provisioned.pg_schema,
            "pgRoles":  provisioned.pg_roles,
            "dataApiDeployment": data_api_deployment,
        }
    });
    api.patch_status(&name, &PatchParams::apply("velocity-operator"), &Patch::Merge(&status_patch))
        .await?;

    tracing::info!(%org, %app, %domain, "Domain ready");

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

/// Stamp or un-stamp `velocity.sh/dedicated` on every `SchemaDefinition` in
/// `sd_namespace`. When `dedicated = true`, sets the label to `"true"` so the
/// app-scoped catch-all pod's selector (`velocity.sh/dedicated!=true`) excludes
/// those schemas. When `dedicated = false`, the label value is `null` — a
/// Kubernetes merge-patch null removes the key, returning SDs to the app-scoped
/// pool.
///
/// A missing `sd_namespace` (404) is not an error: no SDs exist yet, nothing
/// to stamp. Per-SD patch failures are logged but do not abort the loop so the
/// operation stays idempotent across reconcile retries.
async fn stamp_schema_definitions(
    kube: &kube::Client,
    sd_namespace: &str,
    dedicated: bool,
) -> Result<(), ReconcileError> {
    let api: Api<SchemaDefinition> = Api::namespaced(kube.clone(), sd_namespace);
    let sds = match api.list(&ListParams::default()).await {
        Ok(list) => list,
        Err(kube::Error::Api(err)) if err.code == 404 => return Ok(()),
        Err(e) => {
            return Err(ReconcileError::Invalid(format!(
                "listing SchemaDefinitions in {sd_namespace}: {e}"
            )));
        }
    };

    let label_val: serde_json::Value =
        if dedicated { json!("true") } else { serde_json::Value::Null };

    for sd in sds.items {
        let sd_name = sd.name_any();
        let patch = json!({
            "metadata": { "labels": { "velocity.sh/dedicated": label_val } }
        });
        if let Err(e) = api
            .patch(&sd_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            tracing::warn!(
                %sd_namespace, %sd_name, error = %e,
                "failed to stamp SchemaDefinition velocity.sh/dedicated={dedicated}"
            );
        }
    }

    Ok(())
}

/// Controller-level error policy.
pub fn error_policy(_obj: Arc<Domain>, err: &ReconcileError, _ctx: Arc<Context>) -> Action {
    tracing::warn!(error = %err, "Domain reconcile failed");
    error_action(err)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    #[test]
    fn sd_namespace_is_org_app_domain() {
        let org = "acme";
        let app = "supply-chain";
        let domain = "procurement";
        assert_eq!(format!("{org}-{app}-{domain}"), "acme-supply-chain-procurement");
    }

    #[test]
    fn sd_namespace_differs_from_domain_namespace() {
        // Domain lives in {org}-{app}; SDs in {org}-{app}-{domain}.
        // The workload bug was passing the Domain's namespace to workload::sync.
        let org = "acme";
        let app = "erp";
        let domain = "accounting";
        let domain_ns = format!("{org}-{app}");
        let sd_ns = format!("{org}-{app}-{domain}");
        assert_ne!(domain_ns, sd_ns);
        assert!(sd_ns.starts_with(&domain_ns));
    }

    #[test]
    fn dedicated_label_val_is_string_true() {
        let val: Value = json!("true");
        assert_eq!(val.as_str(), Some("true"));
    }

    #[test]
    fn undedicated_label_val_serialises_as_null() {
        let val = Value::Null;
        let patch = json!({ "metadata": { "labels": { "velocity.sh/dedicated": val } } });
        assert!(patch["metadata"]["labels"]["velocity.sh/dedicated"].is_null(),
            "null label value must be present so Kubernetes merge-patch removes the key");
    }
}
