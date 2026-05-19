//! Shared kube-rs glue used by every CRD-touching subcommand.
//!
//! Three reusable pieces:
//!
//! - [`build_client`] honours `--kubeconfig` (or `$KUBECONFIG`), falling
//!   back to the standard discovery chain (in-cluster SA, then
//!   `~/.kube/config`). Same shape as `status.rs` / `reconcile.rs`
//!   were doing in copies — collapsed here so future commands all
//!   land in one path.
//! - [`parse_target`] splits `<namespace>/<name>` or `<name>` for
//!   cluster-scoped kinds.
//! - [`find_resource`] resolves a kind string to an
//!   `ApiResource` + scope through `kube::Discovery`. Case-insensitive,
//!   limited to the `velocity.sh` group so we can never accidentally
//!   touch a non-Velocity CRD.
//! - [`parse_manifests`] reads YAML — file path or `-` for stdin —
//!   and returns one `DynamicObject` per document. Multi-doc YAML
//!   (`---` separators) is supported so an operator can apply a bundle
//!   in one call.

use std::collections::HashSet;
use std::io::Read as _;

use anyhow::{anyhow, Context, Result};
use kube::api::DynamicObject;
use kube::core::{ApiResource, GroupVersionKind};
use kube::discovery::{ApiCapabilities, Scope};
use kube::{Client, Config, Discovery};
use serde::Deserialize as _;

/// API group every Velocity CRD lives in. Centralising this avoids
/// typo-shaped bugs in cmd modules.
pub(crate) const VELOCITY_GROUP: &str = "velocity.sh";

/// Build a kube client honouring `--kubeconfig` if given, otherwise
/// falling back to the standard discovery chain (KUBECONFIG env,
/// `~/.kube/config`, in-cluster service account).
pub(crate) async fn build_client(kubeconfig: Option<&str>) -> Result<Client> {
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

/// Split `<namespace>/<name>` or `<name>`. Returns `(Some(ns), name)` or
/// `(None, name)`. Empty components are rejected.
pub(crate) fn parse_target(s: &str) -> Result<(Option<String>, String)> {
    if let Some((ns, name)) = s.split_once('/') {
        if ns.is_empty() || name.is_empty() {
            return Err(anyhow!(
                "invalid target `{s}` (expected `<namespace>/<name>` or `<name>`)"
            ));
        }
        Ok((Some(ns.to_string()), name.to_string()))
    } else if s.is_empty() {
        Err(anyhow!("empty target"))
    } else {
        Ok((None, s.to_string()))
    }
}

/// Resolve a kind name (case-insensitive) to `(ApiResource, scope)`
/// within the velocity.sh group. Errors with a friendly list of known
/// kinds when the input doesn't match — better than a kube-rs discovery
/// miss that looks like a CLI bug.
pub(crate) fn find_resource(
    discovery: &Discovery,
    kind: &str,
) -> Result<(ApiResource, ApiCapabilities)> {
    let mut known: HashSet<String> = HashSet::new();
    for group in discovery.groups() {
        if group.name() != VELOCITY_GROUP {
            continue;
        }
        for (ar, caps) in group.recommended_resources() {
            known.insert(ar.kind.clone());
            if ar.kind.eq_ignore_ascii_case(kind) {
                return Ok((ar, caps));
            }
        }
    }
    let gvk = GroupVersionKind::gvk(VELOCITY_GROUP, "v1", kind);
    let mut known_sorted: Vec<_> = known.into_iter().collect();
    known_sorted.sort();
    Err(anyhow!(
        "no Velocity CRD found for kind `{kind}` ({}/{} {}). \
         Known kinds: {}",
        gvk.group,
        gvk.version,
        gvk.kind,
        if known_sorted.is_empty() { "(none discovered)".into() } else { known_sorted.join(", ") },
    ))
}

/// Read a YAML manifest from a file path or stdin (`-`), split on `---`
/// document separators, and return one `DynamicObject` per document.
/// Empty documents are skipped (kubectl behaviour).
pub(crate) fn parse_manifests(source: &str) -> Result<Vec<DynamicObject>> {
    let raw = if source == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("reading manifest from stdin")?;
        buf
    } else {
        std::fs::read_to_string(source)
            .with_context(|| format!("reading manifest from {source}"))?
    };

    let mut out: Vec<DynamicObject> = Vec::new();
    for (i, doc) in serde_yaml::Deserializer::from_str(&raw).enumerate() {
        let value: serde_yaml::Value = serde_yaml::Value::deserialize(doc)
            .with_context(|| format!("parsing document #{} of {}", i + 1, source))?;
        // Skip empty docs (kubectl behaviour: a trailing `---` or blank
        // section produces a null Value — silently ignored).
        if value.is_null() {
            continue;
        }
        let obj: DynamicObject = serde_yaml::from_value(value)
            .with_context(|| format!("decoding document #{} of {}", i + 1, source))?;
        if obj.types.is_none() {
            return Err(anyhow!(
                "document #{} of {} is missing `apiVersion`/`kind`",
                i + 1,
                source
            ));
        }
        out.push(obj);
    }
    if out.is_empty() {
        return Err(anyhow!("no manifests found in {source}"));
    }
    Ok(out)
}

/// True when the resource lives inside a namespace (vs cluster-scoped).
/// One-line helper so call sites read better than poking at `caps.scope`.
pub(crate) fn is_namespaced(caps: &ApiCapabilities) -> bool {
    caps.scope == Scope::Namespaced
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
        assert!(parse_target("").is_err());
    }

    #[test]
    fn parse_manifests_single_doc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.yaml");
        std::fs::write(
            &path,
            "apiVersion: velocity.sh/v1\nkind: SchemaDefinition\nmetadata:\n  name: po-v1\n",
        )
        .unwrap();
        let objs = parse_manifests(path.to_str().unwrap()).unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].types.as_ref().unwrap().kind, "SchemaDefinition");
        assert_eq!(objs[0].metadata.name.as_deref(), Some("po-v1"));
    }

    #[test]
    fn parse_manifests_multi_doc_skips_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.yaml");
        std::fs::write(
            &path,
            "---\n\
             apiVersion: velocity.sh/v1\nkind: SchemaDefinition\nmetadata:\n  name: a\n\
             ---\n\
             ---\n\
             apiVersion: velocity.sh/v1\nkind: SchemaDefinition\nmetadata:\n  name: b\n",
        )
        .unwrap();
        let objs = parse_manifests(path.to_str().unwrap()).unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].metadata.name.as_deref(), Some("a"));
        assert_eq!(objs[1].metadata.name.as_deref(), Some("b"));
    }

    #[test]
    fn parse_manifests_rejects_doc_missing_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "metadata:\n  name: orphan\n").unwrap();
        let err = parse_manifests(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("missing `apiVersion`/`kind`"));
    }

    #[test]
    fn parse_manifests_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.yaml");
        std::fs::write(&path, "---\n---\n").unwrap();
        let err = parse_manifests(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("no manifests"));
    }
}
