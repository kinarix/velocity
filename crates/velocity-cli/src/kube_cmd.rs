//! kubectl-style CRUD against Velocity CRDs.
//!
//! Five sibling subcommands, all backed by the same `kube::Discovery`
//! lookup so the CLI doesn't need a static GVK table:
//!
//! - `apply -f <path|->`  — server-side apply, multi-doc YAML.
//! - `get <kind> [name]`  — list or get-one, optional namespace filter.
//! - `describe <kind> <name>` — pretty-print spec + conditions.
//! - `delete <kind> <name>`   — DELETE the resource.
//! - `diff -f <path|->`       — server-side dry-run apply + unified diff
//!   against the current cluster object.
//!
//! Why dynamic instead of typed? The operator owns the typed structs
//! (`SchemaDefinition`, `AuthStrategy`, …); the CLI just needs to push
//! YAML and read status. `DynamicObject` plus discovery lets new CRDs
//! work without a CLI rebuild — the same trick `kubectl` plays.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use kube::api::{DeleteParams, DynamicObject, ListParams, Patch, PatchParams};
use kube::core::ApiResource;
use kube::discovery::ApiCapabilities;
use kube::{Api, Discovery};
use serde_json::Value;

use crate::kube_helpers::{build_client, find_resource, is_namespaced, parse_manifests};
use crate::output::{print, OutputFormat};

const FIELD_MANAGER: &str = "velocity-cli";

// ---------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct ApplyArgs {
    /// Manifest path, or `-` for stdin. YAML; multi-doc (`---`) accepted.
    #[arg(short, long)]
    pub file: String,

    /// Server-side dry-run: validate + return the would-be-stored object
    /// without persisting. Useful in CI gates.
    #[arg(long)]
    pub dry_run: bool,
}

pub(crate) async fn apply(args: ApplyArgs, kubeconfig: &Option<String>) -> Result<()> {
    let manifests = parse_manifests(&args.file)?;
    let client = build_client(kubeconfig.as_deref()).await?;
    let discovery =
        Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;

    let mut pp = PatchParams::apply(FIELD_MANAGER).force();
    if args.dry_run {
        pp = pp.dry_run();
    }

    for obj in manifests {
        let kind = obj
            .types
            .as_ref()
            .map(|t| t.kind.clone())
            .ok_or_else(|| anyhow!("manifest missing kind"))?;
        let name = obj
            .metadata
            .name
            .clone()
            .ok_or_else(|| anyhow!("{kind} manifest missing metadata.name"))?;

        let (ar, caps) = find_resource(&discovery, &kind)?;
        let api = object_api(client.clone(), &ar, &caps, obj.metadata.namespace.as_deref())?;

        let outcome = api
            .patch(&name, &pp, &Patch::Apply(&obj))
            .await
            .with_context(|| format!("applying {kind} {name}"))?;

        let server_ns = outcome.metadata.namespace.as_deref().unwrap_or("<cluster>");
        let suffix = if args.dry_run { " (dry-run)" } else { "" };
        eprintln!("{kind} {server_ns}/{name} applied{suffix}");
    }
    Ok(())
}

// ---------------------------------------------------------------------
// get
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct GetArgs {
    /// Kind: SchemaDefinition, AuthStrategy, ArchivePolicy, RoleBinding,
    /// ApiKey, LogFilterPolicy, LogRoutingPolicy, PurgeRequest,
    /// Organisation, Application, Domain. Case-insensitive.
    pub kind: String,

    /// Optional name. Without it, lists all resources of `kind`.
    pub name: Option<String>,

    /// Filter to a single namespace. Without this, all namespaces are
    /// scanned for namespaced kinds; ignored for cluster-scoped ones.
    #[arg(short, long)]
    pub namespace: Option<String>,
}

pub(crate) async fn get(
    args: GetArgs,
    kubeconfig: &Option<String>,
    output: OutputFormat,
) -> Result<()> {
    let client = build_client(kubeconfig.as_deref()).await?;
    let discovery =
        Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;
    let (ar, caps) = find_resource(&discovery, &args.kind)?;
    let api = object_api(client, &ar, &caps, args.namespace.as_deref())?;

    if let Some(name) = &args.name {
        let obj = api.get(name).await.with_context(|| format!("getting {} {name}", args.kind))?;
        let rows = vec![row_for_object(&obj, is_namespaced(&caps))];
        print(headers(is_namespaced(&caps)), &rows, output);
    } else {
        let list = api
            .list(&ListParams::default())
            .await
            .with_context(|| format!("listing {}", args.kind))?;
        let rows: Vec<Vec<String>> =
            list.items.iter().map(|o| row_for_object(o, is_namespaced(&caps))).collect();
        print(headers(is_namespaced(&caps)), &rows, output);
    }
    Ok(())
}

fn headers(namespaced: bool) -> &'static [&'static str] {
    if namespaced {
        &["NAMESPACE", "NAME", "PHASE", "AGE", "READY"]
    } else {
        &["NAME", "PHASE", "AGE", "READY"]
    }
}

fn row_for_object(obj: &DynamicObject, namespaced: bool) -> Vec<String> {
    let name = obj.metadata.name.clone().unwrap_or_else(|| "<unnamed>".into());
    let age = obj
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.to_string())
        .unwrap_or_else(|| "—".into());
    let (phase, ready) = phase_and_ready(obj);

    if namespaced {
        let ns = obj.metadata.namespace.clone().unwrap_or_else(|| "<none>".into());
        vec![ns, name, phase, age, ready]
    } else {
        vec![name, phase, age, ready]
    }
}

/// Pull `.status.phase` and the Ready condition message out of an
/// arbitrary DynamicObject. Both are best-effort: a freshly-applied CRD
/// that hasn't been reconciled yet has neither, which we report as `—`.
fn phase_and_ready(obj: &DynamicObject) -> (String, String) {
    let status = match obj.data.get("status") {
        Some(Value::Object(m)) => m,
        _ => return ("—".into(), "—".into()),
    };

    let phase = status.get("phase").and_then(Value::as_str).unwrap_or("—").to_string();

    let ready = status
        .get("conditions")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter().find(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    || c.get("kind").and_then(Value::as_str) == Some("Ready")
            })
        })
        .map(|c| {
            let s = c.get("status").and_then(Value::as_str).unwrap_or("?");
            let msg = c.get("message").and_then(Value::as_str).unwrap_or("");
            if msg.is_empty() {
                s.to_string()
            } else {
                format!("{s} ({msg})")
            }
        })
        .unwrap_or_else(|| "—".into());

    (phase, ready)
}

// ---------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct DescribeArgs {
    /// Kind (case-insensitive).
    pub kind: String,
    /// Name. Must already exist.
    pub name: String,
    /// Namespace (omit for cluster-scoped kinds).
    #[arg(short, long)]
    pub namespace: Option<String>,
}

pub(crate) async fn describe(args: DescribeArgs, kubeconfig: &Option<String>) -> Result<()> {
    let client = build_client(kubeconfig.as_deref()).await?;
    let discovery =
        Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;
    let (ar, caps) = find_resource(&discovery, &args.kind)?;
    let api = object_api(client, &ar, &caps, args.namespace.as_deref())?;
    let obj = api
        .get(&args.name)
        .await
        .with_context(|| format!("getting {} {}", args.kind, args.name))?;

    render_describe(&args.kind, &obj);
    Ok(())
}

fn render_describe(kind: &str, obj: &DynamicObject) {
    println!("Kind:        {kind}");
    println!("Name:        {}", obj.metadata.name.as_deref().unwrap_or("<unnamed>"));
    if let Some(ns) = &obj.metadata.namespace {
        println!("Namespace:   {ns}");
    }
    if let Some(t) = &obj.metadata.creation_timestamp {
        println!("Created:     {}", t.0);
    }
    if let Some(labels) = &obj.metadata.labels {
        let velocity_labels: BTreeMap<_, _> =
            labels.iter().filter(|(k, _)| k.starts_with("velocity.sh/")).collect();
        if !velocity_labels.is_empty() {
            println!("Labels:");
            for (k, v) in velocity_labels {
                println!("  {k}: {v}");
            }
        }
    }

    if let Some(spec) = obj.data.get("spec") {
        println!("Spec:");
        for line in serde_yaml::to_string(spec).unwrap_or_else(|_| "<unrenderable>".into()).lines()
        {
            println!("  {line}");
        }
    }

    let Some(Value::Object(status)) = obj.data.get("status") else {
        println!("Status:      <none>");
        return;
    };

    if let Some(phase) = status.get("phase").and_then(Value::as_str) {
        println!("Phase:       {phase}");
    }
    if let Some(arr) = status.get("conditions").and_then(Value::as_array) {
        println!("Conditions:");
        for c in arr {
            let kind =
                c.get("type").or_else(|| c.get("kind")).and_then(Value::as_str).unwrap_or("?");
            let status_v = c.get("status").and_then(Value::as_str).unwrap_or("?");
            let msg = c.get("message").and_then(Value::as_str).unwrap_or("");
            if msg.is_empty() {
                println!("  - {kind}={status_v}");
            } else {
                println!("  - {kind}={status_v}  {msg}");
            }
        }
    }
}

// ---------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct DeleteArgs {
    pub kind: String,
    pub name: String,
    #[arg(short, long)]
    pub namespace: Option<String>,
    /// Skip the confirmation prompt. Required in non-TTY pipelines.
    #[arg(long)]
    pub yes: bool,
}

pub(crate) async fn delete(args: DeleteArgs, kubeconfig: &Option<String>) -> Result<()> {
    if !args.yes && !crate::confirm::confirm(&format!("delete {} {}?", args.kind, args.name))? {
        bail!("aborted");
    }
    let client = build_client(kubeconfig.as_deref()).await?;
    let discovery =
        Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;
    let (ar, caps) = find_resource(&discovery, &args.kind)?;
    let api = object_api(client, &ar, &caps, args.namespace.as_deref())?;

    api.delete(&args.name, &DeleteParams::default())
        .await
        .with_context(|| format!("deleting {} {}", args.kind, args.name))?;
    eprintln!("{} {} deleted", args.kind, args.name);
    Ok(())
}

// ---------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------

#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    /// Manifest path, or `-` for stdin.
    #[arg(short, long)]
    pub file: String,
}

pub(crate) async fn diff(args: DiffArgs, kubeconfig: &Option<String>) -> Result<()> {
    let manifests = parse_manifests(&args.file)?;
    let client = build_client(kubeconfig.as_deref()).await?;
    let discovery =
        Discovery::new(client.clone()).run().await.context("discovering cluster APIs")?;
    let dry_run = PatchParams::apply(FIELD_MANAGER).force().dry_run();

    for obj in manifests {
        let kind = obj
            .types
            .as_ref()
            .map(|t| t.kind.clone())
            .ok_or_else(|| anyhow!("manifest missing kind"))?;
        let name = obj
            .metadata
            .name
            .clone()
            .ok_or_else(|| anyhow!("{kind} manifest missing metadata.name"))?;

        let (ar, caps) = find_resource(&discovery, &kind)?;
        let api = object_api(client.clone(), &ar, &caps, obj.metadata.namespace.as_deref())?;

        // Current may not exist — kubectl-diff renders an "everything added"
        // diff in that case, so we mirror it with an empty current.
        let current_yaml = match api.get(&name).await {
            Ok(o) => render_yaml_for_diff(&o)?,
            Err(kube::Error::Api(e)) if e.code == 404 => String::new(),
            Err(e) => {
                return Err(anyhow!(e).context(format!("getting current {kind} {name}")));
            }
        };

        let server_side = api
            .patch(&name, &dry_run, &Patch::Apply(&obj))
            .await
            .with_context(|| format!("dry-running apply for {kind} {name}"))?;
        let proposed_yaml = render_yaml_for_diff(&server_side)?;

        let ns = obj.metadata.namespace.as_deref().unwrap_or("<cluster>");
        println!("=== {kind} {ns}/{name} ===");
        let mut wrote_anything = false;
        for line in unified_diff(&current_yaml, &proposed_yaml) {
            wrote_anything = true;
            println!("{line}");
        }
        if !wrote_anything {
            println!("(no change)");
        }
    }
    Ok(())
}

/// Strip volatile metadata so the diff focuses on intent, not bookkeeping.
fn render_yaml_for_diff(obj: &DynamicObject) -> Result<String> {
    let mut v = serde_json::to_value(obj).context("serialising object for diff")?;
    if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        for k in [
            "managedFields",
            "resourceVersion",
            "generation",
            "uid",
            "creationTimestamp",
            "selfLink",
        ] {
            meta.remove(k);
        }
    }
    serde_yaml::to_string(&v).context("serialising object as YAML")
}

/// Tiny unified diff — line by line, no context window, prefixes `-`/`+`.
/// Good enough for the kubectl-diff "what's the operator going to change"
/// use case; an external diff tool stays available via `velocity get -o
/// yaml | diff -u …` if a richer view is needed.
fn unified_diff(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines == new_lines {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(old_lines.len() + new_lines.len());
    let max = old_lines.len().max(new_lines.len());
    for i in 0..max {
        match (old_lines.get(i), new_lines.get(i)) {
            (Some(a), Some(b)) if a == b => out.push(format!(" {a}")),
            (Some(a), Some(b)) => {
                out.push(format!("-{a}"));
                out.push(format!("+{b}"));
            }
            (Some(a), None) => out.push(format!("-{a}")),
            (None, Some(b)) => out.push(format!("+{b}")),
            (None, None) => {}
        }
    }
    out
}

// ---------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------

fn object_api(
    client: kube::Client,
    ar: &ApiResource,
    caps: &ApiCapabilities,
    namespace: Option<&str>,
) -> Result<Api<DynamicObject>> {
    if is_namespaced(caps) {
        let ns = namespace.ok_or_else(|| {
            anyhow!("{} is namespaced — supply --namespace <ns> (or set on the manifest)", ar.kind)
        })?;
        Ok(Api::namespaced_with(client, ns, ar))
    } else {
        if namespace.is_some() {
            return Err(anyhow!("{} is cluster-scoped — drop --namespace", ar.kind));
        }
        Ok(Api::all_with(client, ar))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn dyn_obj(status: Value) -> DynamicObject {
        let mut o = DynamicObject::new(
            "po-v1",
            &ApiResource {
                group: "velocity.sh".into(),
                version: "v1".into(),
                api_version: "velocity.sh/v1".into(),
                kind: "SchemaDefinition".into(),
                plural: "schemadefinitions".into(),
            },
        );
        o.data = json!({ "status": status });
        o
    }

    #[test]
    fn phase_and_ready_missing_status() {
        let obj = DynamicObject::new(
            "po-v1",
            &ApiResource {
                group: "velocity.sh".into(),
                version: "v1".into(),
                api_version: "velocity.sh/v1".into(),
                kind: "SchemaDefinition".into(),
                plural: "schemadefinitions".into(),
            },
        );
        let (p, r) = phase_and_ready(&obj);
        assert_eq!(p, "—");
        assert_eq!(r, "—");
    }

    #[test]
    fn phase_and_ready_extracts_ready_condition() {
        let obj = dyn_obj(json!({
            "phase": "Ready",
            "conditions": [
                { "type": "Ready", "status": "True", "message": "Provisioned" },
                { "type": "TableExists", "status": "True" }
            ]
        }));
        let (p, r) = phase_and_ready(&obj);
        assert_eq!(p, "Ready");
        assert!(r.contains("True"));
        assert!(r.contains("Provisioned"));
    }

    #[test]
    fn phase_and_ready_handles_kind_field_variant() {
        // Velocity's own Condition struct uses `kind` instead of `type`
        // (legacy of an early ADR). Both shapes must work.
        let obj = dyn_obj(json!({
            "phase": "Pending",
            "conditions": [ { "kind": "Ready", "status": "False", "message": "WebhookDown" } ]
        }));
        let (p, r) = phase_and_ready(&obj);
        assert_eq!(p, "Pending");
        assert!(r.contains("False"));
        assert!(r.contains("WebhookDown"));
    }

    #[test]
    fn unified_diff_identical_inputs_yield_empty_output() {
        let d = unified_diff("a\nb\nc\n", "a\nb\nc\n");
        assert!(d.is_empty());
    }

    #[test]
    fn unified_diff_marks_changed_and_added_lines() {
        let d = unified_diff("a\nb\nc", "a\nB\nc\nd");
        // Expect: " a", "-b", "+B", " c", "+d"
        assert_eq!(d.len(), 5);
        assert_eq!(d[0], " a");
        assert_eq!(d[1], "-b");
        assert_eq!(d[2], "+B");
        assert_eq!(d[3], " c");
        assert_eq!(d[4], "+d");
    }

    #[test]
    fn unified_diff_handles_pure_addition() {
        let d = unified_diff("", "a\nb");
        assert_eq!(d, vec!["+a".to_string(), "+b".to_string()]);
    }

    #[test]
    fn render_yaml_for_diff_strips_volatile_metadata() {
        let mut obj = DynamicObject::new(
            "po-v1",
            &ApiResource {
                group: "velocity.sh".into(),
                version: "v1".into(),
                api_version: "velocity.sh/v1".into(),
                kind: "SchemaDefinition".into(),
                plural: "schemadefinitions".into(),
            },
        );
        obj.metadata.resource_version = Some("9999".into());
        obj.metadata.uid = Some("abc-uid".into());
        obj.metadata.generation = Some(42);

        let yaml = render_yaml_for_diff(&obj).unwrap();
        assert!(!yaml.contains("resourceVersion"));
        assert!(!yaml.contains("9999"));
        assert!(!yaml.contains("abc-uid"));
        assert!(!yaml.contains("generation"));
    }
}
