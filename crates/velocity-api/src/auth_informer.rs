//! Kube informer feeding [`crate::auth::AuthRegistry`] and priming
//! [`crate::auth::JwksCache`] + claim mappings on [`crate::auth::AuthState`].
//!
//! Mirrors [`crate::informer`] (which watches `SchemaDefinition`) but
//! follows the `AuthStrategy` CRD. Each event:
//!
//! 1. resolves the CRD into [`crate::auth::ResolvedAuthStrategy`]
//! 2. registers it with the registry (lock-free swap on hot read path)
//! 3. registers every issuer with the shared JWKS cache so the first JWT
//!    request after a config change doesn't pay the JWKS round-trip
//! 4. compiles the claim mapping into the per-strategy cache on
//!    [`crate::auth::AuthState`]
//!
//! The `Init` snapshot is buffered the same way the schema informer does it,
//! so a delete that lands while we were disconnected doesn't leave a stale
//! strategy in the registry across reconnects.

use std::sync::Arc;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::watcher::{self, watcher, Event};
use kube::ResourceExt;
use velocity_types::common::NamespacedRef;
use velocity_types::crds::auth::AuthStrategy;

use crate::auth::{AuthRegistry, AuthState, ResolvedAuthStrategy};

pub async fn run(
    registry: Arc<AuthRegistry>,
    auth_state: AuthState,
    client: kube::Client,
    namespace: Option<String>,
) -> anyhow::Result<()> {
    let api: Api<AuthStrategy> = match namespace {
        Some(ns) => Api::namespaced(client, &ns),
        None => Api::all(client),
    };

    let mut stream = watcher(api, watcher::Config::default()).boxed();
    tracing::info!("auth strategy informer started");

    let mut bootstrap: Vec<ResolvedAuthStrategy> = Vec::new();

    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Init) => {
                bootstrap.clear();
                tracing::debug!("auth informer init — buffering snapshot");
            }
            Ok(Event::InitApply(crd)) => {
                if let Some(rs) = resolve(&crd) {
                    bootstrap.push(rs);
                }
            }
            Ok(Event::InitDone) => {
                let count = bootstrap.len();
                let strategies = std::mem::take(&mut bootstrap);
                for strategy in &strategies {
                    apply(&registry, &auth_state, strategy).await;
                }
                tracing::info!(count, "auth informer initial sync complete");
            }
            Ok(Event::Apply(crd)) => {
                if let Some(rs) = resolve(&crd) {
                    tracing::info!(strategy = %rs.key, "auth strategy upserted");
                    apply(&registry, &auth_state, &rs).await;
                }
            }
            Ok(Event::Delete(crd)) => {
                if let Some(reference) = reference_from(&crd) {
                    let key = format!("{}/{}", reference.namespace, reference.name);
                    tracing::info!(strategy = %key, "auth strategy removed");
                    registry.remove(&reference);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "auth informer transient error");
            }
        }
    }

    anyhow::bail!("auth informer stream ended");
}

async fn apply(registry: &AuthRegistry, auth_state: &AuthState, strategy: &ResolvedAuthStrategy) {
    strategy.prime_jwks(&auth_state.jwks).await;
    if let Err(e) = auth_state.prime_strategy(strategy) {
        tracing::warn!(
            strategy = %strategy.key,
            error = %e,
            "auth strategy claim mapping failed to compile — strategy will reject requests",
        );
    }
    // Register after priming so the hot path never sees a strategy whose
    // JWKS/claim-mapping side-tables aren't ready yet.
    registry.upsert(strategy.clone());
}

fn resolve(crd: &AuthStrategy) -> Option<ResolvedAuthStrategy> {
    let reference = reference_from(crd)?;
    Some(ResolvedAuthStrategy::from_spec(&reference, crd.spec.clone()))
}

fn reference_from(crd: &AuthStrategy) -> Option<NamespacedRef> {
    let name = crd.name_any();
    let namespace = crd.namespace()?;
    Some(NamespacedRef { namespace, name })
}
