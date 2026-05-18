//! `AuthRegistry` — parallel to [`crate::SchemaRegistry`], holds resolved
//! `AuthStrategy` CRDs.
//!
//! Why a separate registry: a `SchemaDefinition` references an
//! `AuthStrategy` by `NamespacedRef`. If we embedded the resolved strategy
//! in `ResolvedSchema`, every strategy edit would need to cascade through
//! every dependent schema. Storing strategies in their own `ArcSwap` keeps
//! the two reconcile paths independent — the request hot path pays for two
//! atomic pointer loads instead of one, which is unmeasurable.
//!
//! Phase 2a stores only the bits the middleware needs to verify a JWT and
//! map claims. The full CRD spec is kept around in an `Arc` for the rare
//! cases (audit, debugging) that want the raw shape.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use velocity_types::common::NamespacedRef;
use velocity_types::crds::auth::{AuthStrategySpec, AuthStrategyType, IssuerConfig as CrdIssuerConfig};

use crate::auth::jwks::{IssuerConfig as JwksIssuerConfig, JwksCache};

/// One `AuthStrategy` projected into the form the auth middleware reads.
#[derive(Debug, Clone)]
pub struct ResolvedAuthStrategy {
    /// `{namespace}/{name}` of the source `AuthStrategy` — used for audit
    /// and as the strategy name on the [`crate::Identity`].
    pub key: String,
    /// Which credential scheme this strategy accepts. The middleware
    /// branches on this *before* parsing the `Authorization` header so a
    /// `Bearer` token never reaches the API-key checker (and vice versa).
    pub kind: AuthStrategyType,
    /// JWT issuers configured on this strategy. Indexed by the `iss` claim
    /// value so the middleware can pick the right [`IssuerConfig`] from the
    /// unverified token header before reaching for JWKS.
    pub issuers: HashMap<String, Arc<CrdIssuerConfig>>,
    /// Allowed clock skew on `exp`/`nbf` in seconds. Defaults to 30s.
    pub clock_skew_secs: u32,
    /// ADR-003 — Redis revocation fail-mode. `true` = admit on Redis down,
    /// `false` = deny. The middleware records which mode was applied in
    /// every audit row.
    pub revocation_fail_open: bool,
    /// Composite-only: ordered child strategy refs. Middleware walks the
    /// list, picks the first child whose credential scheme is present on
    /// the request, and runs that child's verification path. Empty for
    /// every non-Composite kind. Resolution to the actual
    /// [`ResolvedAuthStrategy`] happens at request time via the registry
    /// — keeps the data structure flat and lets a composite that names a
    /// child not yet in the registry fail closed at request time rather
    /// than at strategy-load time.
    pub composite_children: Vec<NamespacedRef>,
    /// Full CRD snapshot kept for completeness — the hot path uses the
    /// projected fields above.
    pub spec: Arc<AuthStrategySpec>,
}

impl ResolvedAuthStrategy {
    pub fn from_spec(reference: &NamespacedRef, spec: AuthStrategySpec) -> Self {
        let mut issuers = HashMap::with_capacity(spec.config.issuers.len());
        for cfg in &spec.config.issuers {
            issuers.insert(cfg.issuer.clone(), Arc::new(cfg.clone()));
        }
        let clock_skew_secs = spec.config.clock_skew.unwrap_or(30);
        let revocation_fail_open =
            spec.config.revocation.as_ref().map(|r| r.fail_open).unwrap_or(false);
        let kind = spec.kind;
        let composite_children = spec.config.children.clone();

        Self {
            key: format!("{}/{}", reference.namespace, reference.name),
            kind,
            issuers,
            clock_skew_secs,
            revocation_fail_open,
            composite_children,
            spec: Arc::new(spec),
        }
    }

    /// Register every issuer in this strategy with the shared JWKS cache.
    /// Idempotent — re-registering an issuer with the same URL is a no-op,
    /// re-registering with a new URL triggers a fresh fetch.
    pub async fn prime_jwks(&self, cache: &JwksCache) {
        for cfg in self.issuers.values() {
            cache
                .add_issuer(JwksIssuerConfig {
                    issuer: cfg.issuer.clone(),
                    jwks_url: cfg.jwks_url.clone(),
                })
                .await;
        }
    }
}

fn registry_key(reference: &NamespacedRef) -> String {
    format!("{}/{}", reference.namespace, reference.name)
}

#[derive(Debug, Default)]
struct Inner {
    by_ref: HashMap<String, Arc<ResolvedAuthStrategy>>,
}

#[derive(Debug)]
pub struct AuthRegistry {
    inner: ArcSwap<Inner>,
}

impl AuthRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: ArcSwap::from_pointee(Inner::default()) })
    }

    /// Hot-path read. Returns `None` if the strategy isn't registered yet —
    /// callers should treat that as a config error (the operator should
    /// have populated the registry before the dependent schema went live).
    pub fn resolve(&self, reference: &NamespacedRef) -> Option<Arc<ResolvedAuthStrategy>> {
        self.inner.load().by_ref.get(&registry_key(reference)).cloned()
    }

    pub fn upsert(&self, strategy: ResolvedAuthStrategy) {
        let prev = self.inner.load_full();
        let mut next = Inner { by_ref: prev.by_ref.clone() };
        next.by_ref.insert(strategy.key.clone(), Arc::new(strategy));
        self.inner.store(Arc::new(next));
    }

    pub fn remove(&self, reference: &NamespacedRef) {
        let prev = self.inner.load_full();
        let mut next = Inner { by_ref: prev.by_ref.clone() };
        next.by_ref.remove(&registry_key(reference));
        self.inner.store(Arc::new(next));
    }

    pub fn len(&self) -> usize {
        self.inner.load().by_ref.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.load().by_ref.is_empty()
    }
}

impl Default for AuthRegistry {
    fn default() -> Self {
        Self { inner: ArcSwap::from_pointee(Inner::default()) }
    }
}
