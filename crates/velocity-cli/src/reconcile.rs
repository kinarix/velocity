//! `velocity reconcile` — force the operator to requeue a CRD.
//!
//! Mechanism: patch a `velocity.sh/force-reconcile-at: <rfc3339>`
//! annotation. Any change to the resource bumps its `resourceVersion`,
//! which the controller's informer surfaces as an `Applied` event —
//! exactly what the reconciler subscribes to. No operator code change
//! required.
//!
//! We use a server-side merge patch (`application/merge-patch+json`)
//! so we don't have to read-modify-write — the apiserver merges the
//! annotation in atomically.

use anyhow::{anyhow, Context, Result};
use clap::Args;
use kube::api::{DynamicObject, Patch, PatchParams};
use kube::{Api, Discovery};
use serde_json::json;

use crate::kube_helpers::{build_client, find_resource, is_namespaced, parse_target};

const FORCE_ANNOTATION: &str = "velocity.sh/force-reconcile-at";

#[derive(Debug, Args)]
pub(crate) struct ReconcileArgs {
    /// Resource kind (case-insensitive). One of: Organisation,
    /// Application, Domain, SchemaDefinition, AuthStrategy, RoleBinding,
    /// ApiKey, ArchivePolicy, LogFilterPolicy, LogRoutingPolicy,
    /// PurgeRequest.
    pub kind: String,

    /// `<namespace>/<name>` for namespaced kinds, or `<name>` for
    /// cluster-scoped (Organisation, Application).
    pub target: String,
}

pub(crate) async fn run(args: ReconcileArgs, kubeconfig: &Option<String>) -> Result<()> {
    let (namespace, name) = parse_target(&args.target)?;
    let client = build_client(kubeconfig.as_deref()).await?;

    // Discover the API resource by kind — handles both namespaced and
    // cluster-scoped CRDs uniformly. Using Discovery means we don't
    // have to hard-code the GVK table here; if a new CRD ships, it
    // works without a CLI rebuild.
    let discovery =
        Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;
    let (ar, caps) = find_resource(&discovery, &args.kind)?;

    let now = chrono::Utc::now().to_rfc3339();
    let patch = json!({
        "metadata": {
            "annotations": { FORCE_ANNOTATION: now }
        }
    });

    let api: Api<DynamicObject> = if is_namespaced(&caps) {
        let ns = namespace.ok_or_else(|| {
            anyhow!("{} is namespaced — supply --target as <namespace>/<name>", args.kind)
        })?;
        Api::namespaced_with(client, &ns, &ar)
    } else {
        if namespace.is_some() {
            return Err(anyhow!(
                "{} is cluster-scoped — supply --target as <name> (no namespace)",
                args.kind
            ));
        }
        Api::all_with(client, &ar)
    };

    api.patch(&name, &PatchParams::apply("velocity-cli").force(), &Patch::Merge(&patch))
        .await
        .with_context(|| format!("patching {} {name}", args.kind))?;

    eprintln!("requested reconcile of {} {name} at {now}", args.kind);
    Ok(())
}

