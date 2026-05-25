//! Kube informer feeding the [`SchemaRegistry`].
//!
//! Phase 1 keeps resolution synchronous and trivial — we wrap the CRD spec
//! verbatim into a [`ResolvedSchema`]. Phase 2 will resolve AuthStrategy,
//! ArchivePolicy, RBAC merges, and CEL compilation in this same path.
//!
//! On every reconnect, kube-rs emits a `Restart` event with the full snapshot
//! of CRDs; we use that to atomically replace the registry contents and
//! signal readiness. Until that first `Restart`, `/readyz` returns 503.

use std::sync::Arc;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::watcher::{self, watcher, Event};
use kube::ResourceExt;
use velocity_types::common::SchemaPath;
use velocity_types::crds::SchemaDefinition;

use crate::registry::{ResolvedSchema, SchemaRegistry};

/// Run the kube watcher loop. Returns only on fatal stream termination —
/// callers should treat that as a process-level failure.
///
/// `namespace` and `label_selector` are mutually exclusive (validated by
/// [`crate::ApiConfig`]): namespace → domain-scope (one namespace),
/// label_selector → app-scope (`Api::all` + label filter), neither →
/// cluster-wide (platform mode).
pub async fn run(
    registry: Arc<SchemaRegistry>,
    client: kube::Client,
    namespace: Option<String>,
    label_selector: Option<String>,
) -> anyhow::Result<()> {
    let api: Api<SchemaDefinition> = match namespace {
        Some(ns) => Api::namespaced(client, &ns),
        None => Api::all(client),
    };
    let watcher_config = match label_selector {
        Some(sel) => watcher::Config::default().labels(&sel),
        None => watcher::Config::default(),
    };

    let mut stream = watcher(api, watcher_config).boxed();
    tracing::info!("schema informer started");

    // kube-rs 0.96 splits the old `Restarted(Vec<T>)` event into
    // Init → InitApply* → InitDone. We MUST treat that sequence as a single
    // atomic snapshot — otherwise a schema deleted while the watcher is
    // disconnected would remain in the registry forever, since the
    // post-reconnect init batch won't emit a Delete for it.
    let mut bootstrap: Vec<ResolvedSchema> = Vec::new();

    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Init) => {
                bootstrap.clear();
                tracing::debug!("informer init — buffering snapshot");
            }
            Ok(Event::InitApply(sd)) => {
                if let Some(rs) = resolve(&sd) {
                    bootstrap.push(rs);
                }
            }
            Ok(Event::InitDone) => {
                let count = bootstrap.len();
                registry.replace_all(std::mem::take(&mut bootstrap));
                registry.mark_ready();
                tracing::info!(count, "informer initial sync complete — registry ready");
            }
            Ok(Event::Apply(sd)) => {
                if let Some(rs) = resolve(&sd) {
                    tracing::info!(
                        path = %crate::registry::registry_key(&rs.path),
                        "schema upserted into registry"
                    );
                    registry.upsert(rs);
                }
            }
            Ok(Event::Delete(sd)) => {
                if let Some(path) = path_from_labels(&sd) {
                    tracing::info!(
                        path = %crate::registry::registry_key(&path),
                        "schema removed from registry"
                    );
                    registry.remove(&path);
                }
            }
            Err(e) => {
                // Watcher errors are recoverable — the watcher will reconnect
                // and emit a fresh Init/InitApply/InitDone sequence.
                tracing::warn!(error = %e, "schema informer transient error");
            }
        }
    }

    anyhow::bail!("schema informer stream ended");
}

/// Build a `ResolvedSchema` from the CRD. Returns `None` when the CRD is
/// missing the required labels — the operator should have rejected this at
/// the webhook, but we belt-and-brace here.
fn resolve(sd: &SchemaDefinition) -> Option<ResolvedSchema> {
    let path = path_from_labels(sd)?;
    Some(ResolvedSchema::from_spec(path, sd.spec.clone()))
}

fn path_from_labels(sd: &SchemaDefinition) -> Option<SchemaPath> {
    let labels = sd.labels();
    let org = labels.get("velocity.sh/org")?;
    let app = labels.get("velocity.sh/app")?;
    let domain = labels.get("velocity.sh/domain")?;
    let object = sd.name_any();
    Some(SchemaPath::new(org, app, domain, &object, &sd.spec.version))
}
