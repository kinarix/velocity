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
use kube::core::{ApiResource, GroupVersionKind};
use kube::{Api, Client, Config, Discovery};
use serde_json::json;

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
    let discovery = Discovery::new(client.clone())
        .run()
        .await
        .context("discovering cluster APIs")?;
    let (ar, caps) = find_resource(&discovery, &args.kind)?;
    let is_namespaced = caps.scope == kube::discovery::Scope::Namespaced;

    let now = chrono::Utc::now().to_rfc3339();
    let patch = json!({
        "metadata": {
            "annotations": { FORCE_ANNOTATION: now }
        }
    });

    let api: Api<DynamicObject> = if is_namespaced {
        let ns = namespace.ok_or_else(|| {
            anyhow!(
                "{} is namespaced — supply --target as <namespace>/<name>",
                args.kind
            )
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

    api.patch(
        &name,
        &PatchParams::apply("velocity-cli").force(),
        &Patch::Merge(&patch),
    )
    .await
    .with_context(|| format!("patching {} {name}", args.kind))?;

    eprintln!("requested reconcile of {} {name} at {now}", args.kind);
    Ok(())
}

fn parse_target(s: &str) -> Result<(Option<String>, String)> {
    if let Some((ns, name)) = s.split_once('/') {
        if ns.is_empty() || name.is_empty() {
            return Err(anyhow!("invalid target `{s}` (expected `<namespace>/<name>` or `<name>`)"));
        }
        Ok((Some(ns.to_string()), name.to_string()))
    } else {
        Ok((None, s.to_string()))
    }
}

fn find_resource(
    discovery: &Discovery,
    kind: &str,
) -> Result<(ApiResource, kube::discovery::ApiCapabilities)> {
    // Case-insensitive match against discovered resources in
    // velocity.sh/v1. Returns the first hit; falling back to a
    // cluster-wide search lets `kubectl describe`-style shortnames
    // route correctly without the CLI tracking them.
    for group in discovery.groups() {
        if group.name() != "velocity.sh" {
            continue;
        }
        for (ar, caps) in group.recommended_resources() {
            if ar.kind.eq_ignore_ascii_case(kind) {
                return Ok((ar, caps));
            }
        }
    }
    // Fallback: try as GVK directly so `velocity reconcile pod foo`
    // still produces a coherent error rather than a discovery miss
    // looking like the CLI is broken.
    let gvk = GroupVersionKind::gvk("velocity.sh", "v1", kind);
    Err(anyhow!(
        "no Velocity CRD found for kind `{kind}` (looked for {}/{} {})",
        gvk.group,
        gvk.version,
        gvk.kind,
    ))
}

async fn build_client(kubeconfig: Option<&str>) -> Result<Client> {
    if let Some(path) = kubeconfig {
        let cfg_file = std::fs::read_to_string(path)
            .with_context(|| format!("reading kubeconfig at {path}"))?;
        let kubeconfig: kube::config::Kubeconfig =
            serde_yaml::from_str(&cfg_file).context("parsing kubeconfig YAML")?;
        let config = Config::from_custom_kubeconfig(kubeconfig, &Default::default())
            .await
            .context("building kube config from --kubeconfig")?;
        Client::try_from(config).context("building kube client")
    } else {
        Client::try_default()
            .await
            .context("building kube client (no kubeconfig — using default discovery)")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_target_namespaced() {
        let (ns, name) = parse_target("acme-platform/default").unwrap();
        assert_eq!(ns.as_deref(), Some("acme-platform"));
        assert_eq!(name, "default");
    }

    #[test]
    fn parse_target_cluster_scoped() {
        let (ns, name) = parse_target("acme").unwrap();
        assert_eq!(ns, None);
        assert_eq!(name, "acme");
    }

    #[test]
    fn parse_target_rejects_empty_components() {
        assert!(parse_target("/").is_err());
        assert!(parse_target("ns/").is_err());
        assert!(parse_target("/name").is_err());
    }
}
