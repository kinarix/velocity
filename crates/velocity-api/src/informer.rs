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
pub async fn run(
    registry: Arc<SchemaRegistry>,
    client: kube::Client,
    namespace: Option<String>,
) -> anyhow::Result<()> {
    let api: Api<SchemaDefinition> = match namespace {
        Some(ns) => Api::namespaced(client, &ns),
        None => Api::all(client),
    };

    let mut stream = watcher(api, watcher::Config::default()).boxed();
    tracing::info!("schema informer started");

    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Init) => {
                tracing::debug!("informer init");
            }
            Ok(Event::InitApply(sd)) | Ok(Event::Apply(sd)) => {
                if let Some(rs) = resolve(&sd) {
                    tracing::info!(
                        path = %crate::registry::registry_key(&rs.path),
                        "schema upserted into registry"
                    );
                    registry.upsert(rs);
                }
            }
            Ok(Event::InitDone) => {
                tracing::info!(
                    count = registry.snapshot().len(),
                    "informer initial sync complete — registry ready"
                );
                registry.mark_ready();
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
